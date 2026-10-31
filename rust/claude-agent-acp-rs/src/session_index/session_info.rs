//! A session index row as the client sees it (upstream
//! `session-index/session-info.ts`): the `SessionInfo` of `session/list` and of
//! `_session/list/changes`, with the flat row fields of the RFDs in
//! `_meta.jetbrains.air`.

use serde_json::{json, Map, Value};

use super::activity::{derive_activity, select_cost, ActivityInput, OwnSessionState};
use super::js_text::{js_number, to_iso_string};
use super::live_registry::LiveRecord;
use super::IndexRow;
use crate::air::with_air_meta;

fn iso(ms: Option<f64>) -> Option<String> {
    ms.filter(|v| v.is_finite())
        .map(|v| to_iso_string(v as i64))
}

/// The `SessionInfo` of `row`. `own` is what this connection knows of a session
/// it runs, `live` the registry record of the session.
pub fn session_info_of(
    row: &IndexRow,
    own: Option<&OwnSessionState>,
    live: Option<&LiveRecord>,
    now: f64,
) -> Value {
    let activity = derive_activity(&ActivityInput {
        own,
        live,
        facts: &row.facts,
        transcript_mtime_ms: row.mtime_ms,
        now,
    });
    let cost = select_cost(own);
    let facts = &row.facts;
    // The row fields of the session list extensions RFD and RFD #2161, flat; each
    // is omitted when unknown, except `archived`.
    let mut fields: Vec<(&str, Option<Value>)> = vec![
        ("archived", Some(Value::Bool(row.archived))),
        (
            "lastPromptAt",
            iso(facts.last_prompt_at.map(|v| v as f64)).map(Value::String),
        ),
        ("model", facts.model.clone().map(Value::String)),
        ("forkedFrom", facts.forked_from.clone().map(Value::String)),
        (
            "agentWorktree",
            row.agent_worktree.then_some(Value::Bool(true)),
        ),
        (
            "state",
            activity
                .as_ref()
                .and_then(|a| a.state)
                .map(|s| Value::String(s.as_str().to_string())),
        ),
        (
            "lastTurnEndedAt",
            activity
                .as_ref()
                .and_then(|a| a.last_turn_ended_at.clone())
                .map(Value::String),
        ),
    ];
    fields.push((
        "cost",
        cost.map(|amount| json!({"amount": js_number(amount), "currency": "USD"})),
    ));
    let mut meta: Option<Value> = None;
    for (key, value) in fields {
        if let Some(value) = value {
            meta = Some(with_air_meta(meta.as_ref(), key, value));
        }
    }
    let mut info = Map::new();
    info.insert("sessionId".into(), json!(row.session_id));
    info.insert("cwd".into(), json!(row.cwd));
    info.insert("title".into(), json!(row.title));
    info.insert(
        "updatedAt".into(),
        json!(to_iso_string(row.updated_at_ms as i64)),
    );
    if let Some(meta) = meta {
        info.insert("_meta".into(), meta);
    }
    Value::Object(info)
}

/// What a change of the row is compared by: everything but `updatedAt`, which
/// alone is no change.
pub fn change_signature(info: &Value) -> String {
    json!([
        info.get("sessionId"),
        info.get("cwd"),
        info.get("title"),
        info.get("_meta").unwrap_or(&Value::Null),
    ])
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_index::transcript_scan::TranscriptFacts;

    const SESSION: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";

    /// change signature > is the same for an unmarked row as without the agentWorktree field
    #[test]
    fn the_signature_of_an_unmarked_row_has_no_worktree_field() {
        let at = 1_767_225_600_000.0;
        let row = IndexRow {
            session_id: SESSION.into(),
            cwd: "/repo".into(),
            title: "Fix it".into(),
            updated_at_ms: at,
            order_at_ms: at,
            facts: TranscriptFacts {
                has_messages: true,
                model: Some("claude-model-x".into()),
                agent_worktree: Some(false),
                ..TranscriptFacts::default()
            },
            mtime_ms: at,
            archived: false,
            agent_worktree: false,
        };
        let now = at + 3_600_000.0;
        let info = session_info_of(&row, None, None, now);
        assert!(info["_meta"]["jetbrains"]["air"]
            .get("agentWorktree")
            .is_none());
        assert_eq!(
            change_signature(&info),
            json!([SESSION, "/repo", "Fix it", info["_meta"]]).to_string()
        );
        let marked = IndexRow {
            agent_worktree: true,
            ..row
        };
        let marked_info = session_info_of(&marked, None, None, now);
        assert_eq!(
            marked_info["_meta"]["jetbrains"]["air"]["agentWorktree"],
            true
        );
        assert_ne!(change_signature(&marked_info), change_signature(&info));
    }

    #[test]
    fn a_row_carries_its_air_fields_and_the_cost_of_a_running_session() {
        let at = 1_767_225_600_000.0;
        let row = IndexRow {
            session_id: SESSION.into(),
            cwd: "/repo".into(),
            title: "t".into(),
            updated_at_ms: at,
            order_at_ms: at,
            facts: TranscriptFacts {
                has_messages: true,
                last_prompt_at: Some(at as i64 - 5000),
                forked_from: Some("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb".into()),
                ..TranscriptFacts::default()
            },
            mtime_ms: at,
            archived: true,
            agent_worktree: false,
        };
        let own = OwnSessionState {
            cost_usd: Some(2.0),
            ..OwnSessionState::default()
        };
        let info = session_info_of(&row, Some(&own), None, at);
        let air = &info["_meta"]["jetbrains"]["air"];
        assert_eq!(air["version"], 1);
        assert_eq!(air["archived"], true);
        assert_eq!(air["lastPromptAt"], "2025-12-31T23:59:55.000Z");
        assert_eq!(air["state"], "idle");
        assert_eq!(air["cost"], json!({"amount": 2, "currency": "USD"}));
        assert_eq!(air["forkedFrom"], "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb");
        assert!(air.get("model").is_none());
        assert_eq!(info["updatedAt"], "2026-01-01T00:00:00.000Z");
    }
}
