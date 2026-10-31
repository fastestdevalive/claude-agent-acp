//! The ACP session-store methods `session/list`, `session/delete` and the
//! transcript copy of `session/fork` (rust-air-gaps G6). Thin async adapters
//! over [`crate::session_store`]: the filesystem work runs in
//! `spawn_blocking`, and a failure is mapped to the wire error Node gives
//! (a plain `Error` is `-32603` with `data:{details}`; a bad cursor is
//! `-32602`).

use agent_client_protocol::Error;
use serde_json::{json, Value};

use crate::session_store::{self, StoreError, SESSION_LIST_PAGE_SIZE};

fn to_rpc(error: &StoreError) -> Error {
    match error {
        StoreError::Invalid(message) if message.starts_with("Unknown session/list cursor") => {
            Error::new(-32602, format!("Invalid params: {message}"))
        }
        other => Error::internal_error().data(json!({ "details": other.to_string() })),
    }
}

fn join_error(error: tokio::task::JoinError) -> Error {
    Error::internal_error().data(json!({ "details": error.to_string() }))
}

/// `session/list`: one page of sessions, newest first.
pub async fn list(params: &Value) -> Result<Value, Error> {
    let cwd = params
        .get("cwd")
        .and_then(Value::as_str)
        .map(str::to_string);
    let cursor = params
        .get("cursor")
        .and_then(Value::as_str)
        .map(str::to_string);
    let offset = session_store::parse_cursor(cursor.as_deref()).map_err(|e| to_rpc(&e))?;
    let page = tokio::task::spawn_blocking(move || {
        session_store::list(
            &session_store::projects_root(),
            cwd.as_deref(),
            offset,
            SESSION_LIST_PAGE_SIZE,
        )
    })
    .await
    .map_err(join_error)?;
    let mut result = json!({
        "sessions": page.sessions.iter().map(session_store::SessionRow::to_json).collect::<Vec<_>>(),
    });
    if let (Some(cursor), Some(map)) = (page.next_cursor, result.as_object_mut()) {
        map.insert("nextCursor".into(), json!(cursor));
    }
    Ok(result)
}

/// `session/delete` (non-AIR): the SDK delete; an unknown session succeeds.
pub async fn delete(session_id: String) -> Result<(), Error> {
    tokio::task::spawn_blocking(move || {
        session_store::delete(&session_store::projects_root(), &session_id)
    })
    .await
    .map_err(join_error)?
    .map_err(|e| to_rpc(&e))
}

/// The id of a `session/fork` request's AIR fork point
/// (`_meta.jetbrains.air.fork`, version 1, non-empty `messageId`), if any.
pub fn fork_point_message_id(params: &Value) -> Option<String> {
    let fork = params.pointer("/_meta/jetbrains/air/fork")?;
    if fork.get("version").and_then(Value::as_i64) != Some(1) {
        return None;
    }
    let id = fork.get("messageId").and_then(Value::as_str)?.trim();
    (!id.is_empty()).then(|| id.to_string())
}

/// Copy the latest transcript of `source_id` into a new session under `cwd`'s
/// project; returns the new session id.
pub async fn fork_copy(source_id: String, cwd: String) -> Result<String, Error> {
    tokio::task::spawn_blocking(move || {
        session_store::fork(
            &session_store::projects_root(),
            &source_id,
            &cwd,
            &mut crate::agent::uuid_v4,
        )
    })
    .await
    .map_err(join_error)?
    .map_err(|e| to_rpc(&e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inv_52_fork_point_parse() {
        let meta = |fork: Value| json!({"_meta": {"jetbrains": {"air": {"fork": fork}}}});
        assert_eq!(
            fork_point_message_id(&meta(json!({"version": 1, "messageId": " m1 "}))),
            Some("m1".to_string())
        );
        assert_eq!(
            fork_point_message_id(&meta(json!({"version": 2, "messageId": "m1"}))),
            None
        );
        assert_eq!(
            fork_point_message_id(&meta(json!({"version": 1, "messageId": "  "}))),
            None
        );
        assert_eq!(fork_point_message_id(&json!({})), None);
    }

    #[test]
    fn inv_52_error_mapping() {
        let bad = to_rpc(&StoreError::Invalid(
            "Unknown session/list cursor: x".to_string(),
        ));
        assert_eq!(i32::from(bad.code), -32602);
        let other = to_rpc(&StoreError::NotFound("gone".to_string()));
        assert_eq!(i32::from(other.code), -32603);
    }
}
