//! The AIR session-failure lifecycle of the live consumer
//! (`SessionFailureController.prepare/publish/clear`,
//! `session-failure-extension.ts:260-420`): the revision book, the active set
//! and the recovery policy of every published failure.
//!
//! Only a client that declared the AIR `sessionFailure` capability gets these
//! records; the caller checks that. A published record rides a
//! `session_info_update` whose `_meta.jetbrains.air.sessionFailure` carries it.

use std::collections::HashMap;

use serde_json::{json, Value};

use crate::session_failure::{
    failure_policy, session_failure_meta, ClaudeFailureKind, PublishedSessionFailure,
};

/// When an active failure goes away on its own
/// (`SessionFailureRecoveryPolicy`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recovery {
    Never,
    NextAttempt,
    RealModelSuccess,
    AuthStatus,
    RuntimeRebind,
}

/// `sessionFailureRecoveryPolicy` (`session-failure-extension.ts:231-247`).
fn recovery_policy(kind: ClaudeFailureKind, turn_id: Option<&str>) -> Recovery {
    match kind {
        ClaudeFailureKind::Advisory => Recovery::Never,
        ClaudeFailureKind::AuthRequired => Recovery::AuthStatus,
        ClaudeFailureKind::TransportLost | ClaudeFailureKind::WorkerShutdown => {
            Recovery::RuntimeRebind
        }
        ClaudeFailureKind::QuotaExhausted => Recovery::RealModelSuccess,
        _ if turn_id.is_some() => Recovery::NextAttempt,
        _ => Recovery::RealModelSuccess,
    }
}

/// A prepared failure with the book-keeping the wire record does not carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prepared {
    pub failure: PublishedSessionFailure,
    pub turn_id: Option<String>,
    pub recovery: Recovery,
}

/// What a caller says about a failure it publishes.
#[derive(Debug, Clone, Default)]
pub struct FailureOptions {
    /// The prompt the failure belongs to (`session.activeTurn?.promptUuid`).
    pub turn_id: Option<String>,
    /// Session scoped: no turn id, a fresh incident id.
    pub session_scoped: bool,
    pub title: Option<String>,
    pub details: Option<String>,
    /// A machine-readable refinement of the kind (the `--hide-claude-auth`
    /// guard's reason on an `auth_required` failure).
    pub reason: Option<String>,
    pub warning: bool,
}

/// `SessionFailureState` plus the controller's methods.
#[derive(Debug, Clone)]
pub struct SessionFailures {
    epoch: String,
    revisions: HashMap<String, u64>,
    active: Vec<Prepared>,
    next_incident: u64,
    last_notice: Option<(String, String)>,
}

impl Default for SessionFailures {
    fn default() -> Self {
        Self {
            epoch: crate::agent::uuid_v4(),
            revisions: HashMap::new(),
            active: Vec::new(),
            next_incident: 1,
            last_notice: None,
        }
    }
}

impl SessionFailures {
    /// `prepare`: build the record without delivering it. An advisory reuses
    /// the id of the notice it repeats; an error of a turn is `<turn>:error`;
    /// anything session scoped gets a fresh incident id.
    pub fn prepare(
        &mut self,
        session_id: &str,
        kind: ClaudeFailureKind,
        options: &FailureOptions,
    ) -> Prepared {
        let policy = failure_policy(kind);
        let title = options
            .title
            .clone()
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| policy.fallback_title.to_string());
        let revision_of = |revisions: &HashMap<String, u64>, id: &str| {
            revisions.get(id).copied().unwrap_or(0) + 1
        };
        if kind == ClaudeFailureKind::Advisory {
            let repeated = self
                .last_notice
                .as_ref()
                .filter(|(last_title, _)| *last_title == title)
                .map(|(_, id)| id.clone());
            let id = match repeated {
                Some(id) => id,
                None => {
                    let id = format!("{session_id}:notice:{}:{}", self.epoch, self.next_incident);
                    self.next_incident += 1;
                    id
                }
            };
            return Prepared {
                failure: PublishedSessionFailure {
                    revision: revision_of(&self.revisions, &id),
                    id,
                    kind,
                    category: policy.category,
                    severity: "warning",
                    title,
                    details: options.details.clone().filter(|d| !d.is_empty()),
                    reason: None,
                    actions: policy.actions.iter().map(|a| (*a).to_string()).collect(),
                },
                turn_id: None,
                recovery: recovery_policy(kind, None),
            };
        }
        let turn_id = options.turn_id.clone().filter(|_| !options.session_scoped);
        let id = match &turn_id {
            Some(turn) => format!("{turn}:error"),
            None => {
                let id = format!(
                    "{session_id}:session-error:{}:{}",
                    self.epoch, self.next_incident
                );
                self.next_incident += 1;
                id
            }
        };
        Prepared {
            failure: PublishedSessionFailure {
                revision: revision_of(&self.revisions, &id),
                id,
                kind,
                category: policy.category,
                severity: if options.warning { "warning" } else { "error" },
                title,
                details: options.details.clone().filter(|d| !d.is_empty()),
                reason: options.reason.clone().filter(|r| !r.is_empty()),
                actions: if options.warning {
                    Vec::new()
                } else {
                    policy.actions.iter().map(|a| (*a).to_string()).collect()
                },
            },
            recovery: recovery_policy(kind, turn_id.as_deref()),
            turn_id,
        }
    }

    /// `publish`: prepare the record, record it as active and return the
    /// `session/update` that delivers it (`recordActive` after a successful
    /// emit; the channel send cannot fail the record).
    pub fn publish(
        &mut self,
        session_id: &str,
        kind: ClaudeFailureKind,
        options: &FailureOptions,
    ) -> Value {
        let prepared = self.prepare(session_id, kind, options);
        let update = json!({
            "sessionId": session_id,
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": session_failure_meta(&prepared.failure),
            },
        });
        self.record_active(prepared);
        update
    }

    /// `recordActive`: the revision book and the active set.
    pub fn record_active(&mut self, prepared: Prepared) {
        let failure = &prepared.failure;
        self.revisions.insert(failure.id.clone(), failure.revision);
        self.last_notice = (failure.kind == ClaudeFailureKind::Advisory)
            .then(|| (failure.title.clone(), failure.id.clone()));
        self.active.retain(|active| active.failure.id != failure.id);
        self.active.push(prepared);
    }

    /// `clear`: drop the active failures `should_clear` selects (and forget the
    /// last advisory notice, so a repeat gets a fresh id).
    pub fn clear(&mut self, should_clear: impl Fn(&Prepared) -> bool) {
        self.last_notice = None;
        self.active.retain(|failure| !should_clear(failure));
    }

    /// `restore`: re-publish a failure of the transcript under its historical
    /// `id` (a replayed usage-limit message). The record is an error of the
    /// kind's policy; it stays active only when `active`, and a record that is
    /// not leaves no active entry. Returns the `session/update` that delivers it.
    pub fn restore(
        &mut self,
        session_id: &str,
        id: &str,
        kind: ClaudeFailureKind,
        title: &str,
        active: bool,
    ) -> Value {
        let policy = failure_policy(kind);
        let failure = PublishedSessionFailure {
            id: id.to_string(),
            revision: self.revisions.get(id).copied().unwrap_or(0) + 1,
            kind,
            category: policy.category,
            severity: "error",
            title: title.to_string(),
            details: None,
            reason: None,
            actions: policy.actions.iter().map(|a| (*a).to_string()).collect(),
        };
        let update = json!({
            "sessionId": session_id,
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": session_failure_meta(&failure),
            },
        });
        self.revisions.insert(failure.id.clone(), failure.revision);
        self.active
            .retain(|existing| existing.failure.id != failure.id);
        if active {
            self.active.push(Prepared {
                recovery: recovery_policy(kind, None),
                turn_id: None,
                failure,
            });
        }
        update
    }

    /// `hasActiveSessionError`: whether a session-scoped error of `kind` is
    /// active. A turn-scoped warning of the same kind (an `api_retry` notice)
    /// does not count, and `reason` refines `kind`: an omitted reason matches
    /// only a record without one.
    pub fn has_active_session_error(&self, kind: ClaudeFailureKind, reason: Option<&str>) -> bool {
        self.active.iter().any(|active| {
            active.failure.kind == kind
                && active.failure.severity == "error"
                && active.turn_id.is_none()
                && active.failure.reason.as_deref() == reason
        })
    }

    /// The failures still active, oldest first.
    pub fn active(&self) -> &[Prepared] {
        &self.active
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(id: &str, warning: bool) -> FailureOptions {
        FailureOptions {
            turn_id: Some(id.to_string()),
            warning,
            ..FailureOptions::default()
        }
    }

    /// A turn-scoped record is `<turn>:error` and its revision rises on every
    /// publish; a warning carries no actions.
    #[test]
    fn t2a_turn_scoped_failure_revisions_rise() {
        let mut failures = SessionFailures::default();
        let first = failures.publish("s", ClaudeFailureKind::RateLimited, &turn("p1", true));
        let sf = &first["update"]["_meta"]["jetbrains"]["air"]["sessionFailure"];
        assert_eq!(sf["id"], "p1:error");
        assert_eq!(sf["revision"], 1);
        assert_eq!(sf["severity"], "warning");
        assert_eq!(sf["actions"], json!([]));
        let second = failures.publish("s", ClaudeFailureKind::RateLimited, &turn("p1", false));
        let sf = &second["update"]["_meta"]["jetbrains"]["air"]["sessionFailure"];
        assert_eq!(sf["revision"], 2);
        assert_eq!(sf["severity"], "error");
        assert_eq!(sf["actions"], json!(["retry"]));
        assert_eq!(failures.active().len(), 1, "one record per id");
    }

    /// A session-scoped record gets a fresh incident id each time.
    #[test]
    fn t2a_session_scoped_failure_gets_fresh_incident_ids() {
        let mut failures = SessionFailures::default();
        let options = FailureOptions {
            session_scoped: true,
            turn_id: Some("p1".into()),
            ..FailureOptions::default()
        };
        let a = failures.prepare("s", ClaudeFailureKind::TransportLost, &options);
        let b = failures.prepare("s", ClaudeFailureKind::TransportLost, &options);
        assert_ne!(a.failure.id, b.failure.id);
        assert!(a.failure.id.starts_with("s:session-error:"));
        assert_eq!(a.turn_id, None);
        assert_eq!(a.recovery, Recovery::RuntimeRebind);
    }

    /// An advisory repeating the last notice keeps its id and bumps its
    /// revision; `clear` makes the next one fresh.
    #[test]
    fn t2a_advisory_repeat_reuses_the_notice_id_until_cleared() {
        let mut failures = SessionFailures::default();
        let options = FailureOptions {
            title: Some("Model fallback".into()),
            ..FailureOptions::default()
        };
        let a = failures.publish("s", ClaudeFailureKind::Advisory, &options);
        let b = failures.publish("s", ClaudeFailureKind::Advisory, &options);
        let id =
            |v: &Value| v["update"]["_meta"]["jetbrains"]["air"]["sessionFailure"]["id"].clone();
        assert_eq!(id(&a), id(&b));
        assert_eq!(
            b["update"]["_meta"]["jetbrains"]["air"]["sessionFailure"]["revision"],
            2
        );
        failures.clear(|_| false);
        let c = failures.publish("s", ClaudeFailureKind::Advisory, &options);
        assert_ne!(id(&a), id(&c));
    }

    /// V-15 — `restore` raises the revision of its id each time and keeps the
    /// record active only when asked; an inactive restore drops an older one.
    #[test]
    fn t3a_restore_counts_revisions_and_honours_active() {
        let mut failures = SessionFailures::default();
        let first = failures.restore(
            "s",
            "p1:error",
            ClaudeFailureKind::QuotaExhausted,
            "Limit",
            true,
        );
        let sf = &first["update"]["_meta"]["jetbrains"]["air"]["sessionFailure"];
        assert_eq!(sf["id"], "p1:error");
        assert_eq!(sf["revision"], 1);
        assert_eq!(sf["severity"], "error");
        assert_eq!(failures.active().len(), 1);
        assert_eq!(failures.active()[0].recovery, Recovery::RealModelSuccess);
        let second = failures.restore(
            "s",
            "p1:error",
            ClaudeFailureKind::QuotaExhausted,
            "Limit",
            false,
        );
        assert_eq!(
            second["update"]["_meta"]["jetbrains"]["air"]["sessionFailure"]["revision"],
            2
        );
        assert!(
            failures.active().is_empty(),
            "inactive restore leaves no active record"
        );
    }

    /// V-14 — `has_active_session_error` ignores turn-scoped records and
    /// warnings, and `reason` must match exactly.
    #[test]
    fn t3a_has_active_session_error_matches_kind_scope_and_reason() {
        let mut failures = SessionFailures::default();
        failures.publish("s", ClaudeFailureKind::RateLimited, &turn("p1", true));
        assert!(!failures.has_active_session_error(ClaudeFailureKind::RateLimited, None));
        let options = FailureOptions {
            session_scoped: true,
            reason: Some("subscription".into()),
            ..FailureOptions::default()
        };
        failures.publish("s", ClaudeFailureKind::AuthRequired, &options);
        assert!(failures
            .has_active_session_error(ClaudeFailureKind::AuthRequired, Some("subscription")));
        assert!(!failures.has_active_session_error(ClaudeFailureKind::AuthRequired, None));
        assert!(!failures.has_active_session_error(ClaudeFailureKind::AuthRequired, Some("other")));
    }

    /// `clear` selects by predicate (the turn-boundary and real-model sweeps).
    #[test]
    fn t2a_clear_selects_by_recovery() {
        let mut failures = SessionFailures::default();
        failures.publish("s", ClaudeFailureKind::RateLimited, &turn("p1", true));
        failures.publish("s", ClaudeFailureKind::QuotaExhausted, &turn("p2", false));
        failures.clear(|failure| failure.recovery == Recovery::RealModelSuccess);
        assert_eq!(failures.active().len(), 1);
        assert_eq!(failures.active()[0].recovery, Recovery::NextAttempt);
    }
}
