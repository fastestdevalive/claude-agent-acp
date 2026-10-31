//! The session index of one ACP connection (upstream
//! `session-index/connection.ts`): what the agent does for a client that declared
//! `sessionIndex`, and the archive of an AIR client without it. The agent keeps
//! thin call sites; everything else lives here.
//!
//! Wire contract: docs/air-extensions.md, "Session index".

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};

use agent_client_protocol::Error;
use serde_json::{json, Value};
use tokio::sync::{watch, OwnedSemaphorePermit, Semaphore};

use super::activity::{ActivityState, OwnSessionState};
use super::archive_title::{is_archived_title, stored_title, visible_title};
use super::list_subscriptions::{
    parse_list_subscribe_request, parse_list_unsubscribe_request, ListSubscriptionTiming,
    LIST_SUBSCRIBE_METHOD, LIST_UNSUBSCRIBE_METHOD,
};
use super::service::{
    internal_error, method_not_found, parse_rename_session_request, parse_session_id_request,
    BoxFuture, ListChangesNotifier, LiveTitle, RenameFn, RetitleOptions, ServiceDeps,
    SessionIndexService, SidecarMode, TitleChange, SESSION_ARCHIVE_METHOD, SESSION_RENAME_METHOD,
    SESSION_UNARCHIVE_METHOD,
};
use crate::air::{
    with_air_meta, AirClient, AIR_SESSION_ARCHIVE_CAPABILITY, AIR_SESSION_INDEX_CAPABILITY,
    AIR_SESSION_LIST_SUBSCRIBE_CAPABILITY, AIR_SESSION_RENAME_CAPABILITY,
    AIR_SESSION_WORKTREES_CAPABILITY,
};
use crate::session::{ExplicitPersist, Session};
use crate::session_titles::ExplicitPublish;

type LogError = Arc<dyn Fn(&str, &str) + Send + Sync>;

/// What this connection knows of the sessions it runs, for the rows of the session
/// index (`Session.lastSessionState`, `lastTurnEndedAt`, `lastTurnFailed`,
/// `lastTotalCostUsd` upstream). The session side publishes into it; the list
/// reads it. A watch channel keeps the map without a lock and lets the list
/// subscriptions learn that a session changed.
#[derive(Clone)]
pub struct OwnSessionBoard {
    tx: Arc<watch::Sender<HashMap<String, OwnSessionState>>>,
    titles: Arc<watch::Sender<HashMap<String, TitleState>>>,
    /// The title changes of a session in request order (upstream chains them in
    /// `exclusive`): a change reads the title state only after the earlier one
    /// stored its title.
    order: Arc<watch::Sender<HashMap<String, Arc<Semaphore>>>>,
    /// The incarnation of every session wired here: a session closed and loaded
    /// again under the same id is a new one, and what the old one still reports
    /// (a turn that settles late, its end) must not touch the new one's state.
    incarnations: Arc<watch::Sender<HashMap<String, Incarnation>>>,
    next_incarnation: Arc<AtomicU64>,
}

/// One run of a session under its id, from the moment it is wired to the board.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Incarnation(u64);

type PendingTurn =
    Pin<Box<dyn Future<Output = Result<OwnedSemaphorePermit, tokio::sync::AcquireError>> + Send>>;

/// A place in a session's title change order, taken when the request arrived.
/// `acquire` is declared before `hold` so a ticket dropped unused releases its
/// queue position before the hold tidies the gate.
pub struct TitleOrderTicket {
    acquire: Option<PendingTurn>,
    hold: TitleOrderHold,
}

impl TitleOrderTicket {
    /// Waits for the earlier changes of the session to end.
    pub async fn wait(self) -> Option<TitleOrderHold> {
        let Self { acquire, mut hold } = self;
        if let Some(acquire) = acquire {
            hold.permit = Some(acquire.await.ok()?);
        }
        hold.permit.is_some().then_some(hold)
    }
}

/// A session's turn in the title change order; dropping it lets the next change run.
pub struct TitleOrderHold {
    board: OwnSessionBoard,
    session_id: String,
    permit: Option<OwnedSemaphorePermit>,
    gate: Option<Arc<Semaphore>>,
}

impl Drop for TitleOrderHold {
    fn drop(&mut self) {
        drop(self.permit.take());
        drop(self.gate.take());
        self.board.order.send_modify(|gates| {
            let idle = gates
                .get(&self.session_id)
                .is_some_and(|gate| Arc::strong_count(gate) == 1 && gate.available_permits() == 1);
            if idle {
                gates.remove(&self.session_id);
            }
        });
    }
}

/// The title state of a loaded session that a retitle reads (`SessionTitles.storedTitle`
/// and `shownTitle` upstream).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TitleState {
    /// The title last stored through the CLI, as stored (with the archive prefix of
    /// an archived session): the title the CLI holds.
    pub stored: Option<String>,
    /// The title last published, as the client shows it.
    pub shown: Option<String>,
}

/// Upstream keeps at most 1024 failed queries (`connection.ts:169`), so the
/// board does not grow one entry per session ever prompted.
const MAX_FAILED_QUERIES: usize = 1024;

/// Drops the oldest remembered failed query once the cap is exceeded.
fn bound_failed_queries(map: &mut HashMap<String, OwnSessionState>) {
    let failed = map
        .values()
        .filter(|own| own.query_failed_at.is_some())
        .count();
    if failed <= MAX_FAILED_QUERIES {
        return;
    }
    let oldest = map
        .iter()
        .filter_map(|(id, own)| own.query_failed_at.map(|at| (id.clone(), at)))
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(id, _)| id);
    if let Some(id) = oldest {
        map.remove(&id);
    }
}

impl Default for OwnSessionBoard {
    fn default() -> Self {
        Self::new()
    }
}

impl OwnSessionBoard {
    pub fn new() -> Self {
        Self {
            tx: Arc::new(watch::channel(HashMap::new()).0),
            titles: Arc::new(watch::channel(HashMap::new()).0),
            order: Arc::new(watch::channel(HashMap::new()).0),
            incarnations: Arc::new(watch::channel(HashMap::new()).0),
            next_incarnation: Arc::new(AtomicU64::new(1)),
        }
    }

    /// A session is wired here: it replaces any earlier incarnation of the id.
    pub fn begin(&self, session_id: &str) -> Incarnation {
        let incarnation = Incarnation(self.next_incarnation.fetch_add(1, Ordering::Relaxed));
        self.incarnations.send_modify(|map| {
            map.insert(session_id.to_string(), incarnation);
        });
        incarnation
    }

    /// The incarnation of `session_id` that runs now, if any.
    pub fn current(&self, session_id: &str) -> Option<Incarnation> {
        self.incarnations.borrow().get(session_id).copied()
    }

    /// The incarnation ended by itself (its actor ended): its row shows what the
    /// registry and the transcript tell, unless a newer incarnation of the id
    /// runs or the teardown cleared it already.
    pub fn retire(&self, session_id: &str, incarnation: Incarnation) {
        if self.current(session_id) == Some(incarnation) {
            self.forget(session_id);
        }
    }

    /// Waits for the session's earlier title changes to end (tokio's semaphore is
    /// FIFO) and holds the session's turn until the guard is dropped.
    pub async fn title_order(&self, session_id: &str) -> Option<TitleOrderHold> {
        self.reserve_title_order(session_id).wait().await
    }

    /// Takes the session's place in the title change order NOW, synchronously: a
    /// semaphore queues a waiter only when its acquire is first polled, and a
    /// change that runs in its own spawned task is polled in no particular order.
    /// The request handler reserves before it spawns, so two pipelined changes
    /// keep request order.
    pub fn reserve_title_order(&self, session_id: &str) -> TitleOrderTicket {
        let mut gate = Arc::new(Semaphore::new(1));
        self.order.send_modify(|gates| {
            gate = gates
                .entry(session_id.to_string())
                .or_insert_with(|| Arc::new(Semaphore::new(1)))
                .clone();
        });
        let mut acquire: PendingTurn = Box::pin(gate.clone().acquire_owned());
        // One poll queues the waiter (or takes a free gate); a no-op waker is fine,
        // the real one is registered by the next poll.
        let first = acquire
            .as_mut()
            .poll(&mut Context::from_waker(futures::task::noop_waker_ref()));
        let (acquire, permit) = match first {
            Poll::Ready(permit) => (None, permit.ok()),
            Poll::Pending => (Some(acquire), None),
        };
        TitleOrderTicket {
            acquire,
            hold: TitleOrderHold {
                board: self.clone(),
                session_id: session_id.to_string(),
                permit,
                gate: Some(gate),
            },
        }
    }

    pub fn title_state(&self, session_id: &str) -> TitleState {
        self.titles
            .borrow()
            .get(session_id)
            .cloned()
            .unwrap_or_default()
    }

    /// A title the CLI took (`rememberStoredTitle`).
    pub fn remember_stored_title(&self, session_id: &str, title: &str) {
        self.titles.send_modify(|map| {
            map.entry(session_id.to_string()).or_default().stored = Some(title.to_string());
        });
    }

    /// The title last published for the session.
    pub fn note_shown_title(&self, session_id: &str, title: &str) {
        self.titles.send_modify(|map| {
            map.entry(session_id.to_string()).or_default().shown = Some(title.to_string());
        });
    }

    /// Changes the state of `session_id` (created idle when it was not there).
    pub fn update(&self, session_id: &str, change: impl FnOnce(&mut OwnSessionState)) {
        self.tx
            .send_modify(|map| change(map.entry(session_id.to_string()).or_default()));
    }

    /// Changes the state of `session_id` only when it has one: what a session
    /// reports late must not bring back a row that was forgotten.
    fn update_existing(&self, session_id: &str, change: impl FnOnce(&mut OwnSessionState)) {
        self.tx
            .send_if_modified(|map| match map.get_mut(session_id) {
                Some(own) => {
                    change(own);
                    true
                }
                None => false,
            });
    }

    /// The `session_state_changed` of upstream's `noteSessionState`: an idle after
    /// a non-idle ends a turn, a running after a non-running starts one.
    pub fn note_session_state(&self, session_id: &str, state: ActivityState, now_ms: f64) {
        self.update(session_id, |own| {
            let previous = own.state;
            if state == ActivityState::Idle && previous != Some(ActivityState::Idle) {
                own.last_turn_ended_at = Some(now_ms);
            }
            if state == ActivityState::Running && previous != Some(ActivityState::Running) {
                own.last_turn_failed = false;
            }
            own.state = Some(state);
        });
    }

    /// A prompt of the session was sent: the turn runs until its replies settle.
    pub fn turn_started(&self, session_id: &str) -> Option<Incarnation> {
        let incarnation = self.current(session_id)?;
        self.update(session_id, |own| {
            own.prompts_in_flight += 1;
            own.query_failed_at = None;
            if own.state != Some(ActivityState::Running) {
                own.last_turn_failed = false;
            }
            own.state = Some(ActivityState::Running);
        });
        Some(incarnation)
    }

    /// The reply of a prompt settled. The session is idle again once every prompt
    /// in flight has; `failed`: the turn ended with an error (not a cancel).
    /// Ignored when `incarnation` is no longer the session's: the session was
    /// closed (or replaced) while the turn ran.
    pub fn turn_settled(&self, session_id: &str, incarnation: Incarnation, failed: bool) {
        if self.current(session_id) != Some(incarnation) {
            return;
        }
        let now = system_now_ms();
        self.update_existing(session_id, |own| {
            own.prompts_in_flight = own.prompts_in_flight.saturating_sub(1);
            if failed {
                own.last_turn_failed = true;
            }
            if own.prompts_in_flight == 0 {
                if own.state != Some(ActivityState::Idle) {
                    own.last_turn_ended_at = Some(now);
                }
                own.state = Some(ActivityState::Idle);
            }
        });
    }

    /// `total_cost_usd` of the last result.
    pub fn note_cost(&self, session_id: &str, cost_usd: f64) {
        self.update(session_id, |own| own.cost_usd = Some(cost_usd));
    }

    /// [`Self::note_cost`] of one incarnation: ignored once it is not the session's.
    pub fn note_cost_of(&self, session_id: &str, incarnation: Incarnation, cost_usd: f64) {
        if self.current(session_id) == Some(incarnation) {
            self.update_existing(session_id, |own| own.cost_usd = Some(cost_usd));
        }
    }

    /// The last turn failed with an error (not a cancel).
    pub fn note_turn_failed(&self, session_id: &str) {
        self.update(session_id, |own| own.last_turn_failed = true);
    }

    /// The query of a session that ran here failed: its row shows `error` until
    /// its transcript changes.
    pub fn note_query_failed(&self, session_id: &str, now_ms: f64) {
        self.tx.send_modify(|map| {
            map.entry(session_id.to_string())
                .or_default()
                .query_failed_at = Some(now_ms);
            bound_failed_queries(map);
        });
    }

    /// A turn of a session started here: an earlier failed query of it no longer
    /// counts.
    pub fn note_turn_started(&self, session_id: &str) {
        self.update(session_id, |own| own.query_failed_at = None);
    }

    /// The session is closed here: its row shows what the registry and the
    /// transcript tell. A failed query is remembered.
    pub fn forget(&self, session_id: &str) {
        self.incarnations.send_modify(|map| {
            map.remove(session_id);
        });
        self.titles.send_modify(|map| {
            map.remove(session_id);
        });
        self.tx.send_modify(|map| {
            if let Some(own) = map.get(session_id) {
                match own.query_failed_at {
                    Some(at) => {
                        let kept = OwnSessionState {
                            query_failed_at: Some(at),
                            ..OwnSessionState::default()
                        };
                        map.insert(session_id.to_string(), kept);
                    }
                    None => {
                        map.remove(session_id);
                    }
                }
            }
            bound_failed_queries(map);
        });
    }

    pub fn get(&self, session_id: &str) -> Option<OwnSessionState> {
        self.tx.borrow().get(session_id).cloned()
    }

    /// A receiver that fires whenever any session changes.
    pub fn subscribe(&self) -> watch::Receiver<HashMap<String, OwnSessionState>> {
        self.tx.subscribe()
    }
}

fn system_now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64() * 1000.0)
}

/// Where the connection's index reads, derived from the environment snapshot.
#[derive(Clone, Debug)]
pub struct IndexPaths {
    /// `<config>/projects`.
    pub projects_root: PathBuf,
    /// `<config>/sessions`.
    pub registry_dir: PathBuf,
}

/// The loaded session a mutation reaches through its CLI, with the callbacks the
/// agent gives it.
pub struct LoadedSession {
    pub session: Session,
    pub session_id: String,
    /// The cwd the session runs in (`Session.cwd`).
    pub cwd: String,
    /// Called once the archive closed the session, before anything else can ask
    /// for it (the agent forgets the registry entries of the closed session).
    pub on_closed: Option<Arc<dyn Fn() + Send + Sync>>,
}

pub struct SessionIndexConnection {
    paths: IndexPaths,
    log_error: LogError,
    /// The session index of a client that declared `sessionIndex`. `None` for every
    /// other client, which keeps the session list, delete and the watchers exactly
    /// as before.
    service: Option<SessionIndexService>,
    /// The archive behind `session/delete` of an AIR client without
    /// `sessionIndex`: the same writes, without a list or a watcher.
    archiver: Option<SessionIndexService>,
    board: OwnSessionBoard,
    /// Sends `_session/list/changes` to the client.
    notify: Option<ListChangesNotifier>,
    /// The timing of the list subscriptions (tests shorten it).
    subscription_timing: ListSubscriptionTiming,
}

impl SessionIndexConnection {
    pub fn new(paths: IndexPaths, log_error: LogError) -> Self {
        Self {
            paths,
            log_error,
            service: None,
            archiver: None,
            board: OwnSessionBoard::new(),
            notify: None,
            subscription_timing: ListSubscriptionTiming::default(),
        }
    }

    pub fn board(&self) -> &OwnSessionBoard {
        &self.board
    }

    /// Shortens the timing of the list subscriptions (tests).
    pub fn set_subscription_timing(&mut self, timing: ListSubscriptionTiming) {
        self.subscription_timing = timing;
    }

    fn service_deps(&self, with_own: bool) -> ServiceDeps {
        let mut deps = ServiceDeps::new(self.log_error.clone());
        if with_own {
            let board = self.board.clone();
            deps.own_session_state = Some(Arc::new(move |id| board.get(id)));
            deps.own_changes = Some(self.board.subscribe());
            deps.notify_list_changes = self.notify.clone();
            deps.subscription_timing = self.subscription_timing;
        }
        deps
    }

    /// Sets the index up for the client of `initialize`. ACP v2 does not route the
    /// session index methods yet (this adapter speaks v1).
    pub fn negotiate(&mut self, air: Option<&AirClient>) {
        if let Some(previous) = self.service.take() {
            previous.dispose();
        }
        if air.is_some_and(|air| air.supports(AIR_SESSION_INDEX_CAPABILITY)) {
            self.service = Some(SessionIndexService::new(
                self.paths.projects_root.clone(),
                self.paths.registry_dir.clone(),
                self.service_deps(true),
            ));
        }
    }

    /// The AIR capabilities the agent advertises for the index: the index itself,
    /// archive, rename, the list subscription and the `agentWorktree` row field,
    /// all only to a client that declared `sessionIndex`.
    pub fn capabilities(&self) -> Vec<&'static str> {
        if self.service.is_some() {
            vec![
                AIR_SESSION_INDEX_CAPABILITY,
                AIR_SESSION_ARCHIVE_CAPABILITY,
                AIR_SESSION_RENAME_CAPABILITY,
                AIR_SESSION_LIST_SUBSCRIBE_CAPABILITY,
                AIR_SESSION_WORKTREES_CAPABILITY,
            ]
        } else {
            Vec::new()
        }
    }

    /// A stored title as the client shows it: an AIR client gets the title of an
    /// archived session without the archive prefix, as in its list; every other
    /// client the title as stored.
    pub fn client_title(is_air_client: bool, title: &str) -> String {
        if is_air_client && is_archived_title(Some(title)) {
            visible_title(title)
        } else {
            title.to_string()
        }
    }

    /// An AIR client archives with session/delete: its archived sessions (an
    /// archived custom title) stay hidden from the old list, as when the delete
    /// removed them.
    pub fn hide_archived<T>(
        is_air_client: bool,
        sessions: Vec<T>,
        custom_title: impl Fn(&T) -> Option<&str>,
    ) -> Vec<T> {
        if !is_air_client {
            return sessions;
        }
        sessions
            .into_iter()
            .filter(|session| !is_archived_title(custom_title(session)))
            .collect()
    }

    pub fn has_service(&self) -> bool {
        self.service.is_some()
    }

    pub fn service(&self) -> Option<&SessionIndexService> {
        self.service.as_ref()
    }

    fn require_service(&self, method: &str) -> Result<SessionIndexService, Error> {
        self.service.clone().ok_or_else(|| method_not_found(method))
    }

    /// `session/list` of a `sessionIndex` client: the future for another client is
    /// `None`.
    pub fn list(&self, params: Value) -> Option<BoxFuture<Result<Value, Error>>> {
        let service = self.service.clone()?;
        let board = self.board.clone();
        Some(Box::pin(async move {
            service.list(&params, &move |id| board.get(id)).await
        }))
    }

    /// The query of a session that ran here failed (not a cancel).
    pub fn on_query_failed(&self, session_id: &str, now_ms: f64) {
        self.board.note_query_failed(session_id, now_ms);
    }

    /// A turn of a session started here.
    pub fn on_turn_started(&self, session_id: &str) {
        self.board.note_turn_started(session_id);
    }

    /// The session was torn down here.
    pub fn on_teardown(&self, session_id: &str) {
        self.board.forget(session_id);
    }

    /// Runs `fork`, the handler of a `session/fork` request of `source_id`. The
    /// session list holds back the forks of the source until the response is out.
    pub fn fork_started(&self, source_id: &str) {
        if let Some(service) = &self.service {
            service.fork_started(source_id);
        }
    }

    /// The `session/fork` of `source_id` copied the transcript into `fork_id`.
    pub fn on_fork_created(&self, source_id: &str, fork_id: &str) {
        if let Some(service) = &self.service {
            service.fork_created(source_id, fork_id);
        }
    }

    /// The response of the `session/fork` of `source_id` is out.
    pub fn fork_answered(&self, source_id: &str) {
        if let Some(service) = &self.service {
            service.fork_answered(source_id);
        }
    }

    /// `_session/list/subscribe`.
    pub fn subscribe_list(&self, params: &Value) -> Result<BoxFuture<Result<Value, Error>>, Error> {
        let cwd = parse_list_subscribe_request(params)?;
        let service = self.require_service(LIST_SUBSCRIBE_METHOD)?;
        Ok(Box::pin(async move { service.subscribe_list(&cwd).await }))
    }

    /// `_session/list/unsubscribe`. Idempotent.
    pub fn unsubscribe_list(&self, params: &Value) -> Result<Value, Error> {
        let subscription_id = parse_list_unsubscribe_request(params)?;
        let service = self.require_service(LIST_UNSUBSCRIBE_METHOD)?;
        service.unsubscribe_list(&subscription_id);
        Ok(json!({}))
    }

    /// The connection ends: the subscriptions stop.
    pub fn dispose(&self) {
        if let Some(service) = &self.service {
            service.dispose();
        }
    }

    /// Where `_session/list/changes` goes (the agent's connection to the client).
    pub fn set_notifier(&mut self, notify: ListChangesNotifier) {
        self.notify = Some(notify);
    }

    /// `_session/rename`: names a session; no generated title replaces it. An
    /// archived session stays archived.
    pub fn rename(
        &self,
        params: &Value,
        loaded: Option<LoadedSession>,
        _publish: Arc<dyn Fn(Value) + Send + Sync>,
    ) -> Result<BoxFuture<Result<Value, Error>>, Error> {
        // The router parses the params before the handler looks for the service.
        let request = parse_rename_session_request(params)?;
        let service = self.require_service(SESSION_RENAME_METHOD)?;
        let board = self.board.clone();
        let order = board.reserve_title_order(&request.session_id);
        Ok(Box::pin(async move {
            let session_id = request.session_id.clone();
            // As the CLI keeps it, and never with the archive prefix: a rename does
            // not change the archive state.
            let title = stored_title(&request.title, false, &session_id);
            let fallback = title.clone();
            // The title as stored: an archived one is cut with its prefix.
            let publish = ExplicitPublish::FromStored(Box::new(move |stored| match stored {
                Some(stored) => visible_title(stored),
                None => fallback.clone(),
            }));
            retitle(
                order,
                &service,
                loaded.as_ref(),
                &session_id,
                TitleChange::Rename(title),
                publish,
                RetitleOptions {
                    may_be_unwritten: true,
                    sidecar: SidecarMode::Always,
                },
            )
            .await?;
            Ok(json!({}))
        }))
    }

    /// `_session/archive`: hides a session from the default list and stops it.
    /// Idempotent; the session need not be loaded. A session loaded here is
    /// interrupted, retitled through its CLI while that still runs, then closed as
    /// `session/close` closes it.
    pub fn archive(
        &self,
        params: &Value,
        loaded: Option<LoadedSession>,
        publish: Arc<dyn Fn(Value) + Send + Sync>,
    ) -> Result<BoxFuture<Result<Value, Error>>, Error> {
        let request = parse_session_id_request(params)?;
        let service = self.require_service(SESSION_ARCHIVE_METHOD)?;
        let board = self.board.clone();
        let log = self.log_error.clone();
        let order = board.reserve_title_order(&request.session_id);
        // The run this archive closes: a load of the id while the archive runs is
        // a newer incarnation, whose state the archive must not wipe.
        let incarnation = board.current(&request.session_id);
        Ok(Box::pin(async move {
            let session_id = request.session_id;
            let is_loaded = loaded.is_some();
            // No turn goes on after the archive: the interrupt goes to the CLI
            // ahead of the rename.
            if let Some(loaded) = &loaded {
                let _ = loaded.session.cancel().await;
            }
            let unwritten = loaded
                .as_ref()
                .is_some_and(|_| is_unwritten(&board, &session_id));
            retitle(
                order,
                &service,
                loaded.as_ref(),
                &session_id,
                TitleChange::Archive(true),
                ExplicitPublish::None,
                RetitleOptions {
                    may_be_unwritten: unwritten,
                    sidecar: SidecarMode::Existing,
                },
            )
            .await?;
            if let Some(loaded) = loaded {
                loaded.session.close().await;
                if let Some(on_closed) = &loaded.on_closed {
                    on_closed();
                }
                if let Some(incarnation) = incarnation {
                    board.retire(&session_id, incarnation);
                }
                // The closed CLI may have lost or overwritten the agent name it was
                // still to write: the transcripts are archived without it once
                // more, as a best effort, since the CLI took the archived title.
                if let Err(e) = service
                    .retitle(
                        &session_id,
                        TitleChange::Archive(true),
                        RetitleOptions {
                            may_be_unwritten: true,
                            sidecar: SidecarMode::Existing,
                        },
                        None,
                    )
                    .await
                {
                    log(
                        &format!("archiving {session_id} after its close failed"),
                        &e.message,
                    );
                }
            }
            report_archived(&publish, &session_id, true, is_loaded);
            Ok(json!({}))
        }))
    }

    /// `_session/unarchive`. Idempotent; it does not load the session.
    pub fn unarchive(
        &self,
        params: &Value,
        loaded: Option<LoadedSession>,
        publish: Arc<dyn Fn(Value) + Send + Sync>,
    ) -> Result<BoxFuture<Result<Value, Error>>, Error> {
        let request = parse_session_id_request(params)?;
        let service = self.require_service(SESSION_UNARCHIVE_METHOD)?;
        let board = self.board.clone();
        let order = board.reserve_title_order(&request.session_id);
        Ok(Box::pin(async move {
            let session_id = request.session_id;
            let is_loaded = loaded.is_some();
            let unwritten = loaded
                .as_ref()
                .is_some_and(|_| is_unwritten(&board, &session_id));
            retitle(
                order,
                &service,
                loaded.as_ref(),
                &session_id,
                TitleChange::Archive(false),
                ExplicitPublish::None,
                RetitleOptions {
                    may_be_unwritten: unwritten,
                    sidecar: SidecarMode::Existing,
                },
            )
            .await?;
            report_archived(&publish, &session_id, false, is_loaded);
            Ok(json!({}))
        }))
    }

    /// `session/delete` after the loaded session (if any) was torn down:
    /// - A `sessionIndex` client deletes for real: every transcript, whoever else
    ///   has the session open, as the CLI does.
    /// - Another AIR client uses delete to mark a session done, and may reopen it
    ///   later: the adapter archives it instead, so the transcript survives.
    /// - `None` for every other client: the SDK delete (the agent's old path).
    pub fn delete_session(
        &mut self,
        session_id: String,
        was_loaded: bool,
        is_air_client: bool,
    ) -> Option<BoxFuture<Result<(), Error>>> {
        if let Some(service) = self.service.clone() {
            return Some(Box::pin(async move {
                service.delete(&session_id, was_loaded).await
            }));
        }
        if is_air_client {
            let archiver = self.air_archiver();
            return Some(Box::pin(async move {
                archiver.archive_instead_of_delete(&session_id).await
            }));
        }
        None
    }

    fn air_archiver(&mut self) -> SessionIndexService {
        if let Some(archiver) = &self.archiver {
            return archiver.clone();
        }
        let archiver = SessionIndexService::new(
            self.paths.projects_root.clone(),
            self.paths.registry_dir.clone(),
            self.service_deps(false),
        );
        self.archiver = Some(archiver.clone());
        archiver
    }
}

/// A new session this connection runs that may have no transcript yet: it resumed
/// no stored conversation and no turn of it has ended. A session whose query ended,
/// or that has history, needs its transcript.
fn is_unwritten(board: &OwnSessionBoard, session_id: &str) -> bool {
    board
        .get(session_id)
        .is_none_or(|own| !own.resumed_from_history && own.last_turn_ended_at.is_none())
}

/// Tells the client the archive state of a session that was loaded on this
/// connection (`session_info_update` with `_meta.jetbrains.air.archived`, RFD
/// #2161's `archived` field), also once the archive closed it.
fn report_archived(
    publish: &Arc<dyn Fn(Value) + Send + Sync>,
    session_id: &str,
    archived: bool,
    loaded: bool,
) {
    if !loaded {
        return;
    }
    publish(json!({
        "sessionId": session_id,
        "update": {
            "sessionUpdate": "session_info_update",
            "_meta": with_air_meta(None, "archived", Value::Bool(archived)),
        },
    }));
}

/// Runs `change` on `session_id`: through the loaded session's title state when
/// it runs here (`SessionTitles.setExplicitTitle`, upstream
/// `connection.ts:313-357`), else directly on its transcripts.
///
/// `publish` is the title the session publishes after the change, if any; the
/// update is on the wire before the caller's response (`await
/// client.sessionUpdate`). `persist` reads the session's title state only once a
/// title generation in flight has ended, so a generated title can never land
/// after the client's.
async fn retitle(
    order: TitleOrderTicket,
    service: &SessionIndexService,
    loaded: Option<&LoadedSession>,
    session_id: &str,
    change: TitleChange,
    publish: ExplicitPublish,
    options: RetitleOptions,
) -> Result<(), Error> {
    // The session's earlier title changes end first, so the snapshot below holds
    // the title they stored (upstream reads it inside its lock). The place in the
    // order was taken when the request arrived.
    let _turn = order.wait().await;
    let Some(loaded) = loaded else {
        // No CLI runs it here, so a session without a transcript is unknown
        // (`live !== undefined && options.mayBeUnwritten` upstream).
        service
            .retitle(
                session_id,
                change,
                RetitleOptions {
                    may_be_unwritten: false,
                    sidecar: options.sidecar,
                },
                None,
            )
            .await?;
        return Ok(());
    };
    let session = loaded.session.clone();
    let cwd = loaded.cwd.clone();
    let id = session_id.to_string();
    let service = service.clone();
    // A persist that failed keeps its error (code and data), which the String
    // error of the title API would otherwise lose.
    let failure: Arc<OnceLock<Error>> = Arc::new(OnceLock::new());
    let slot = failure.clone();
    let persist_session = session.clone();
    let persist: ExplicitPersist = Box::new(move || {
        Box::pin(async move {
            let snapshot = persist_session.title_snapshot().await;
            let live = snapshot.map(|snapshot| LiveTitle {
                cwd,
                rename: rename_fn(persist_session.clone(), id.clone()),
                stored: Box::new(move || snapshot.stored.clone()),
                remember: Box::new({
                    let session = persist_session.clone();
                    move |title| session.remember_stored_title(title)
                }),
                shown: snapshot.shown,
            });
            let may_be_unwritten = options.may_be_unwritten && live.is_some();
            match service
                .retitle(
                    &id,
                    change,
                    RetitleOptions {
                        may_be_unwritten,
                        sidecar: options.sidecar,
                    },
                    live,
                )
                .await
            {
                Ok(stored) => Ok(stored),
                Err(error) => {
                    let message = error.message.clone();
                    let _ = slot.set(error);
                    Err(message)
                }
            }
        })
    });
    match session.set_explicit_title(publish, persist).await {
        Ok(()) => {
            // The published title precedes the response, as upstream's awaited
            // `client.sessionUpdate` does.
            session.flush_updates().await;
            Ok(())
        }
        Err(message) => Err(failure
            .get()
            .cloned()
            .unwrap_or_else(|| internal_error(&message))),
    }
}

/// `rename_session` through the CLI of a loaded session (`query.renameSession`).
fn rename_fn(session: Session, session_id: String) -> RenameFn {
    Box::new(move |title| {
        Box::pin(async move {
            let response = session
                .control()
                .send_request(json!({
                    "subtype": "rename_session",
                    "title": title,
                    "source": "host",
                    "session_id": session_id,
                }))
                .await
                .map_err(|e| internal_error(&e.to_string()))?;
            if response
                .pointer("/response/subtype")
                .and_then(Value::as_str)
                == Some("error")
            {
                let message = response
                    .pointer("/response/error")
                    .and_then(Value::as_str)
                    .unwrap_or("rename_session failed");
                return Err(internal_error(message));
            }
            Ok(())
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// session list subscription > forgets a closed session's own state and keeps the map bounded
    #[test]
    fn the_own_state_is_cleared_on_teardown_and_bounded() {
        let board = OwnSessionBoard::new();
        let incarnation = board.begin("live");
        board.note_session_state("live", ActivityState::Running, 1.0);
        board.note_cost_of("live", incarnation, 1.5);
        assert_eq!(board.get("live").and_then(|own| own.cost_usd), Some(1.5));
        board.forget("live");
        assert!(
            board.get("live").is_none(),
            "a closed session keeps no state or cost"
        );
        // A failed query survives its close (upstream's `failedQueries`).
        board.note_query_failed("failed", 5.0);
        board.forget("failed");
        assert_eq!(
            board.get("failed").and_then(|own| own.query_failed_at),
            Some(5.0)
        );
        // At most 1024 failures are remembered (`connection.ts:169`), oldest
        // first: the board must not grow one entry per session ever prompted.
        for i in 0..(MAX_FAILED_QUERIES + 8) {
            board.note_query_failed(&format!("s{i}"), 100.0 + i as f64);
        }
        assert_eq!(
            board.subscribe().borrow().len(),
            MAX_FAILED_QUERIES,
            "the failed-query map is bounded"
        );
        assert!(board.get("s0").is_none(), "the oldest failure is dropped");
        assert!(board.get(&format!("s{}", MAX_FAILED_QUERIES + 7)).is_some());
    }

    /// session rename > a later title change of a session reads the title state only after the earlier one ended
    #[tokio::test]
    async fn title_changes_of_a_session_run_in_request_order() {
        let board = OwnSessionBoard::new();
        let first = tokio::time::timeout(std::time::Duration::from_secs(5), board.title_order("a"))
            .await
            .expect("first turn")
            .expect("hold");
        let second_board = board.clone();
        let second = tokio::spawn(async move { second_board.title_order("a").await.is_some() });
        // Another session is not held up.
        let other = tokio::time::timeout(std::time::Duration::from_secs(5), board.title_order("b"))
            .await
            .expect("other session")
            .expect("hold");
        drop(other);
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !second.is_finished(),
            "the second change waits for the first"
        );
        drop(first);
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(5), second)
                .await
                .expect("second turn")
                .expect("task")
        );
        assert!(
            board.order.borrow().is_empty(),
            "an idle session keeps no gate"
        );
    }

    /// T3B 0a: pipelined title changes keep request order even when their tasks are polled in reverse
    #[tokio::test]
    async fn pipelined_title_changes_keep_request_order() {
        for _ in 0..50 {
            let board = OwnSessionBoard::new();
            let log = Arc::new(watch::channel(Vec::<u8>::new()).0);
            // Both places are taken in request order, synchronously, before either
            // task exists; the tasks are then started in the opposite order.
            let a = board.reserve_title_order("s");
            let b = board.reserve_title_order("s");
            let spawn = |ticket: TitleOrderTicket, mark: u8| {
                let log = log.clone();
                tokio::spawn(async move {
                    let hold = ticket.wait().await;
                    log.send_modify(|v| v.push(mark));
                    drop(hold);
                })
            };
            let second = spawn(b, 2);
            tokio::task::yield_now().await;
            let first = spawn(a, 1);
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                let _ = first.await;
                let _ = second.await;
            })
            .await
            .expect("both changes end");
            assert_eq!(*log.borrow(), vec![1, 2]);
            assert!(board.order.borrow().is_empty());
        }
    }

    /// T3B 0a: a ticket dropped unused frees its place and its gate
    #[tokio::test]
    async fn a_dropped_title_ticket_frees_the_queue() {
        let board = OwnSessionBoard::new();
        let first = board.reserve_title_order("s");
        let second = board.reserve_title_order("s");
        drop(second);
        let hold = first.wait().await.expect("hold");
        drop(hold);
        assert!(board.order.borrow().is_empty());
    }

    /// T3B 0b: late reports of a forgotten session bring nothing back
    #[test]
    fn late_reports_never_resurrect_a_forgotten_row() {
        let board = OwnSessionBoard::new();
        let old = board.begin("s");
        board.turn_started("s");
        board.update("s", |_| {});
        // The row is dropped while the incarnation record stays (as after a
        // failed-query cleanup): a settle or a cost must not recreate it.
        board.tx.send_modify(|map| {
            map.remove("s");
        });
        board.turn_settled("s", old, false);
        board.note_cost_of("s", old, 1.0);
        assert!(board.get("s").is_none());
    }

    /// T3B 0b: an archive retires the incarnation it closed, not a session loaded under the same id since
    #[test]
    fn an_archive_leaves_a_session_loaded_since_alone() {
        let board = OwnSessionBoard::new();
        let archived = board.begin("s");
        // The client loads the id again while the archive's close() runs.
        let reloaded = board.begin("s");
        board.turn_started("s");
        board.retire("s", archived);
        assert_eq!(board.current("s"), Some(reloaded));
        assert_eq!(board.get("s").map(|own| own.prompts_in_flight), Some(1));
    }

    /// session list subscription > a turn that settles after its session was closed leaves no state behind
    #[test]
    fn a_late_turn_settle_after_the_close_recreates_nothing() {
        let board = OwnSessionBoard::new();
        let old = board.begin("s");
        assert_eq!(board.turn_started("s"), Some(old));
        board.forget("s");
        board.turn_settled("s", old, false);
        assert!(
            board.get("s").is_none(),
            "no stale idle entry after the close"
        );
        assert!(board.current("s").is_none());
        // A session that is not wired starts no turn.
        assert_eq!(board.turn_started("s"), None);
        assert!(board.get("s").is_none());
    }

    /// session list subscription > the end of an old session does not wipe a session loaded under the same id
    #[test]
    fn the_end_of_an_old_incarnation_leaves_the_new_one_alone() {
        let board = OwnSessionBoard::new();
        let old = board.begin("s");
        let new = board.begin("s");
        assert_ne!(old, new);
        assert_eq!(board.turn_started("s"), Some(new));
        board.note_cost_of("s", new, 2.0);
        board.retire("s", old);
        board.turn_settled("s", old, true);
        board.note_cost_of("s", old, 9.0);
        let own = board.get("s").expect("the new incarnation keeps its state");
        assert_eq!(own.cost_usd, Some(2.0));
        assert_eq!(own.prompts_in_flight, 1);
        assert!(!own.last_turn_failed);
        board.retire("s", new);
        assert!(board.get("s").is_none());
    }
}
