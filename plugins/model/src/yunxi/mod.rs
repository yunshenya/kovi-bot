mod affect_store;
pub(crate) mod bridge;
pub(crate) mod core_model;
pub(crate) mod delivery;
mod delivery_ledger;
pub(crate) mod events;
mod executive_store;
mod goal_store;
mod identity_store;
pub(crate) mod intrinsic_runtime;
pub(crate) mod memory_migration;
mod memory_store;
mod mind_runtime;
mod mind_store;
mod open_loop_scheduler;
mod open_loop_store;
mod owner_lock;
pub(crate) mod proactive;
pub(crate) mod qq;
mod relation_store;
mod schema;

use affect_store::PostgresAffectStore;
use anyhow::{Context, Result};
use delivery_ledger::PostgresDeliveryLedger;
use executive_store::PostgresExecutiveStore;
use goal_store::PostgresGoalStore;
use identity_store::PostgresIdentityStore;
use kovi::tokio::sync::{Mutex as AsyncMutex, Notify, RwLock as AsyncRwLock, RwLockReadGuard};
use memory_store::PostgresMemoryStore;
use mind_runtime::{
    MindCandidateContext, MindCandidates, MindContextServices, MindDeliveryPermit,
    MindErasureGuard, MindRuntime,
};
pub(crate) use mind_runtime::{MindProactiveReference, MindProactiveSignals};
use mind_store::PostgresMindStore;
use open_loop_store::PostgresOpenLoopStore;
use relation_store::PostgresRelationStore;
use std::sync::{
    Arc, OnceLock, RwLock,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
use yunxi_core::{
    AffectState, ConversationId, IdentityStore, MindDataErasure, PersonId, RelationState,
};

const MIND_ERASURE_MAX_ATTEMPTS: usize = 3;
const EXECUTIVE_SAVE_TIMEOUT: Duration = Duration::from_secs(2);
const EXECUTIVE_ERASURE_TIMEOUT: Duration = Duration::from_secs(5);
const EXECUTIVE_ERASURE_MAX_ATTEMPTS: usize = 3;
const EXECUTIVE_ERASURE_RETRY_DELAY: Duration = Duration::from_millis(250);
const EXECUTIVE_SAVE_RETRY_DELAY: Duration = Duration::from_secs(1);

static IDENTITY_STORE: OnceLock<Arc<PostgresIdentityStore>> = OnceLock::new();
static OPEN_LOOP_STORE: OnceLock<Arc<PostgresOpenLoopStore>> = OnceLock::new();
static MEMORY_STORE: OnceLock<Arc<PostgresMemoryStore>> = OnceLock::new();
static AFFECT_STORE: OnceLock<Arc<PostgresAffectStore>> = OnceLock::new();
static RELATION_STORE: OnceLock<Arc<PostgresRelationStore>> = OnceLock::new();
static GOAL_STORE: OnceLock<Arc<PostgresGoalStore>> = OnceLock::new();
static DELIVERY_LEDGER: OnceLock<Arc<PostgresDeliveryLedger>> = OnceLock::new();
static MIND_STORE: OnceLock<Arc<PostgresMindStore>> = OnceLock::new();
static MIND_RUNTIME: OnceLock<Arc<MindRuntime>> = OnceLock::new();
static EXECUTIVE_STORE: OnceLock<Arc<PostgresExecutiveStore>> = OnceLock::new();
static EXECUTIVE_BOOTSTRAP: OnceLock<Option<yunxi_core::ExecutiveSnapshot>> = OnceLock::new();
static EXECUTIVE_SAVE_LOCK: OnceLock<Arc<AsyncMutex<()>>> = OnceLock::new();
static EXECUTIVE_SAVE_STATE: OnceLock<Arc<AsyncMutex<ExecutiveSaveState>>> = OnceLock::new();
static EXECUTIVE_SAVE_WORKER: OnceLock<Arc<ExecutiveSaveWorker>> = OnceLock::new();
static EXECUTIVE_SAVE_WORKER_STARTED: AtomicBool = AtomicBool::new(false);
static CORE_BRIDGE: OnceLock<Arc<bridge::CoreBridge>> = OnceLock::new();
static EXECUTIVE_CONTROLLER: OnceLock<yunxi_core::ExecutiveController> = OnceLock::new();
static DELIVERY_ROUTE_LOCK: AsyncRwLock<()> = AsyncRwLock::const_new(());
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OwnerQqRoute {
    Unconfigured,
    Unavailable,
    Resolved(i64),
}

static OWNER_QQ_ROUTE: OnceLock<RwLock<OwnerQqRoute>> = OnceLock::new();

struct ExecutiveSaveWorker {
    notify: Notify,
}

#[derive(Debug, Default)]
struct ExecutiveSaveState {
    dirty: bool,
    requested_version: u64,
    erasure_epoch: u64,
    erasure_blocked: bool,
    erasure_start_version: u64,
}

#[must_use]
pub(crate) struct CanonicalOwnerRouteGuard {
    _route_guard: RwLockReadGuard<'static, ()>,
}

pub(crate) enum CanonicalOwnerAuthorization {
    Unconfigured,
    Denied,
    Authorized(CanonicalOwnerRouteGuard),
}

pub(crate) async fn pin_delivery_routes() -> RwLockReadGuard<'static, ()> {
    DELIVERY_ROUTE_LOCK.read().await
}

pub(crate) async fn initialize_database() -> Result<()> {
    if IDENTITY_STORE.get().is_some()
        && OPEN_LOOP_STORE.get().is_some()
        && MEMORY_STORE.get().is_some()
        && AFFECT_STORE.get().is_some()
        && RELATION_STORE.get().is_some()
        && GOAL_STORE.get().is_some()
        && DELIVERY_LEDGER.get().is_some()
        && MIND_STORE.get().is_some()
        && MIND_RUNTIME.get().is_some()
        && EXECUTIVE_STORE.get().is_some()
        && EXECUTIVE_BOOTSTRAP.get().is_some()
    {
        return Ok(());
    }

    let pool = crate::memory::MEMORY_MANAGER
        .database_pool()
        .cloned()
        .context("PostgreSQL 连接池尚未初始化")?;
    if IDENTITY_STORE.get().is_none() {
        let store = Arc::new(PostgresIdentityStore::new(pool.clone()));
        store.initialize_schema().await?;
        let _ = IDENTITY_STORE.set(store);
    }
    if DELIVERY_LEDGER.get().is_none() {
        let ledger = Arc::new(PostgresDeliveryLedger::new(pool.clone()));
        ledger.initialize_schema().await?;
        let _ = DELIVERY_LEDGER.set(ledger);
    }
    initialize_owner_route().await;
    if OPEN_LOOP_STORE.get().is_none() {
        let store = Arc::new(PostgresOpenLoopStore::new(pool.clone()));
        store.initialize_schema().await?;
        let _ = OPEN_LOOP_STORE.set(store);
    }
    if MEMORY_STORE.get().is_none() {
        let identities = IDENTITY_STORE
            .get()
            .cloned()
            .context("Yunxi identity store 尚未初始化")?;
        let store = Arc::new(PostgresMemoryStore::new(
            Arc::clone(&crate::memory::MEMORY_MANAGER),
            identities,
            pool.clone(),
        ));
        store.initialize_schema().await?;
        let _ = MEMORY_STORE.set(store);
    }
    if AFFECT_STORE.get().is_none() {
        let store = Arc::new(PostgresAffectStore::new(pool.clone()));
        store.initialize_schema().await?;
        let _ = AFFECT_STORE.set(store);
    }
    if RELATION_STORE.get().is_none() {
        let store = Arc::new(PostgresRelationStore::new(pool.clone()));
        store.initialize_schema().await?;
        let _ = RELATION_STORE.set(store);
    }
    if GOAL_STORE.get().is_none() {
        let store = Arc::new(PostgresGoalStore::new(pool.clone()));
        store.initialize_schema().await?;
        let _ = GOAL_STORE.set(store);
    }
    if MIND_STORE.get().is_none() {
        let store = Arc::new(PostgresMindStore::new(pool.clone()));
        store.initialize_schema().await?;
        store.seed_self_model_if_absent().await?;
        let _ = MIND_STORE.set(store);
    }
    if MIND_RUNTIME.get().is_none() {
        let store = MIND_STORE
            .get()
            .cloned()
            .context("Yunxi Mind store 尚未初始化")?;
        let memory: Arc<dyn yunxi_core::MemoryStore> = MEMORY_STORE
            .get()
            .cloned()
            .context("Yunxi memory store 尚未初始化")?;
        let open_loops: Arc<dyn yunxi_core::OpenLoopStore> = OPEN_LOOP_STORE
            .get()
            .cloned()
            .context("Yunxi open-loop store 尚未初始化")?;
        let goals: Arc<dyn yunxi_core::GoalStore> = GOAL_STORE
            .get()
            .cloned()
            .context("Yunxi goal store 尚未初始化")?;
        let runtime = Arc::new(
            MindRuntime::new(store.services(), crate::config::get().mind().clone())?
                .with_context_services(MindContextServices::new(memory, open_loops, goals)),
        );
        let _ = MIND_RUNTIME.set(runtime);
    }
    if EXECUTIVE_STORE.get().is_none() {
        let store = Arc::new(PostgresExecutiveStore::new(pool));
        store.initialize_schema().await?;
        let bootstrap = match store.load_bootstrap().await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                // A malformed or temporarily unreadable Executive row must
                // never prevent deterministic Core, Reminder, or erasure
                // startup. The next bounded turn can write a fresh snapshot.
                kovi::log::warn!("Yunxi Executive bootstrap was discarded: {error}");
                None
            }
        };
        let _ = EXECUTIVE_STORE.set(store);
        let _ = EXECUTIVE_BOOTSTRAP.set(bootstrap);
    }
    Ok(())
}

async fn initialize_owner_route() {
    let _ = refresh_owner_route().await;
}

async fn resolve_owner_route_authoritatively() -> OwnerQqRoute {
    let Some(owner_uuid) = crate::config::get().identity().owner_person_id() else {
        return OwnerQqRoute::Unconfigured;
    };
    let Some(store) = IDENTITY_STORE.get() else {
        return OwnerQqRoute::Unavailable;
    };
    match store
        .qq_external_identities_for_person(yunxi_core::PersonId::from_uuid(owner_uuid))
        .await
    {
        Ok(ids) if ids.len() == 1 => ids[0]
            .parse::<i64>()
            .ok()
            .filter(|id| *id > 0)
            .map_or(OwnerQqRoute::Unavailable, OwnerQqRoute::Resolved),
        Ok(ids) => {
            kovi::log::warn!(
                "canonical Yunxi owner must have exactly one QQ identity, found {}",
                ids.len()
            );
            OwnerQqRoute::Unavailable
        }
        Err(error) => {
            kovi::log::warn!("canonical Yunxi owner QQ route lookup failed: {error}");
            OwnerQqRoute::Unavailable
        }
    }
}

fn cache_owner_route(route: OwnerQqRoute) {
    let cache = OWNER_QQ_ROUTE.get_or_init(|| RwLock::new(OwnerQqRoute::Unavailable));
    if let Ok(mut cached) = cache.write() {
        *cached = route;
    }
}

fn cached_owner_route() -> Option<OwnerQqRoute> {
    OWNER_QQ_ROUTE.get().map(|cache| {
        cache
            .read()
            .map_or(OwnerQqRoute::Unavailable, |route| *route)
    })
}

/// Whether a QQ user is the configured canonical owner. `Some(false)` means
/// the canonical owner is configured and this user is not it; `None` means the
/// canonical owner is not configured and callers may apply legacy fallback.
pub(crate) fn canonical_owner_matches(user_id: i64) -> Option<bool> {
    match cached_owner_route() {
        Some(OwnerQqRoute::Resolved(owner)) => Some(owner == user_id),
        Some(OwnerQqRoute::Unavailable) => Some(false),
        Some(OwnerQqRoute::Unconfigured) => None,
        None => crate::config::get()
            .identity()
            .owner_person_id()
            .map(|_| false),
    }
}

pub(crate) fn canonical_owner_qq_id() -> Option<Option<i64>> {
    match cached_owner_route() {
        Some(OwnerQqRoute::Resolved(owner)) => Some(Some(owner)),
        Some(OwnerQqRoute::Unavailable) => Some(None),
        Some(OwnerQqRoute::Unconfigured) => None,
        None if crate::config::get().identity().owner_person_id().is_some() => Some(None),
        None => None,
    }
}

/// Re-read the canonical owner mapping from authoritative identity storage.
/// Security-sensitive pre-commit and administrator checks use this instead of
/// trusting the process cache, which may have been populated before an unlink.
pub(crate) async fn canonical_owner_matches_authoritative(user_id: i64) -> Option<bool> {
    match refresh_owner_route().await {
        OwnerQqRoute::Resolved(owner) => Some(owner == user_id),
        OwnerQqRoute::Unavailable => Some(false),
        OwnerQqRoute::Unconfigured => None,
    }
}

pub(crate) async fn canonical_owner_qq_id_authoritative() -> Option<Option<i64>> {
    match refresh_owner_route().await {
        OwnerQqRoute::Resolved(owner) => Some(Some(owner)),
        OwnerQqRoute::Unavailable => Some(None),
        OwnerQqRoute::Unconfigured => None,
    }
}

/// Revalidate the canonical owner and pin its identity route through a caller's
/// outgoing commit. The caller must drop the returned guard before transport.
pub(crate) async fn authorize_canonical_owner(user_id: i64) -> CanonicalOwnerAuthorization {
    let route_guard = DELIVERY_ROUTE_LOCK.read().await;
    match refresh_owner_route_while_locked().await {
        OwnerQqRoute::Resolved(owner) if owner == user_id => {
            CanonicalOwnerAuthorization::Authorized(CanonicalOwnerRouteGuard {
                _route_guard: route_guard,
            })
        }
        OwnerQqRoute::Unconfigured => CanonicalOwnerAuthorization::Unconfigured,
        OwnerQqRoute::Resolved(_) | OwnerQqRoute::Unavailable => {
            CanonicalOwnerAuthorization::Denied
        }
    }
}

async fn refresh_owner_route() -> OwnerQqRoute {
    let _route_guard = DELIVERY_ROUTE_LOCK.read().await;
    refresh_owner_route_while_locked().await
}

async fn refresh_owner_route_while_locked() -> OwnerQqRoute {
    let route = resolve_owner_route_authoritatively().await;
    cache_owner_route(route);
    route
}

#[allow(dead_code)]
pub(crate) fn identity_store() -> Option<Arc<PostgresIdentityStore>> {
    IDENTITY_STORE.get().cloned()
}

#[allow(dead_code)]
pub(crate) fn open_loop_store() -> Option<Arc<PostgresOpenLoopStore>> {
    OPEN_LOOP_STORE.get().cloned()
}

#[allow(dead_code)]
pub(crate) fn memory_store() -> Option<Arc<PostgresMemoryStore>> {
    MEMORY_STORE.get().cloned()
}

#[allow(dead_code)]
pub(crate) fn affect_store() -> Option<Arc<PostgresAffectStore>> {
    AFFECT_STORE.get().cloned()
}

#[allow(dead_code)]
pub(crate) fn relation_store() -> Option<Arc<PostgresRelationStore>> {
    RELATION_STORE.get().cloned()
}

#[allow(dead_code)]
pub(crate) fn goal_store() -> Option<Arc<PostgresGoalStore>> {
    GOAL_STORE.get().cloned()
}

pub(crate) fn delivery_ledger() -> Option<Arc<PostgresDeliveryLedger>> {
    DELIVERY_LEDGER.get().cloned()
}

pub(crate) fn mind_store() -> Option<Arc<PostgresMindStore>> {
    MIND_STORE.get().cloned()
}

pub(crate) fn mind_runtime() -> Option<Arc<MindRuntime>> {
    MIND_RUNTIME.get().cloned()
}

pub(crate) fn executive_store() -> Option<Arc<PostgresExecutiveStore>> {
    EXECUTIVE_STORE.get().cloned()
}

pub(crate) fn executive_bootstrap_snapshot() -> Option<yunxi_core::ExecutiveSnapshot> {
    EXECUTIVE_BOOTSTRAP.get().and_then(Clone::clone)
}

/// Request persistence of the latest bounded Executive state.
///
/// The request is deliberately coalesced. A turn must not wait behind a
/// database round trip, and a busy runtime must not create one Tokio task per
/// event. The single worker below snapshots the controller only after taking
/// the shared operation lock, then checks the version again after the write.
pub(crate) async fn persist_executive_snapshot() -> Result<()> {
    if EXECUTIVE_STORE.get().is_none() {
        return Ok(());
    }
    let Some(controller) = EXECUTIVE_CONTROLLER.get() else {
        return Ok(());
    };
    let state = executive_save_state();
    let mut state = state.lock().await;
    let version = controller.version();
    state.requested_version = state.requested_version.max(version);
    state.dirty = true;
    if state.erasure_blocked {
        return Err(anyhow::anyhow!(
            "Yunxi Executive persistence is blocked by an incomplete data erasure"
        ));
    }
    drop(state);
    wake_executive_save_worker();
    Ok(())
}

fn executive_save_state() -> Arc<AsyncMutex<ExecutiveSaveState>> {
    EXECUTIVE_SAVE_STATE
        .get_or_init(|| Arc::new(AsyncMutex::new(ExecutiveSaveState::default())))
        .clone()
}

fn wake_executive_save_worker() {
    let worker = EXECUTIVE_SAVE_WORKER
        .get_or_init(|| {
            Arc::new(ExecutiveSaveWorker {
                notify: Notify::new(),
            })
        })
        .clone();
    if !EXECUTIVE_SAVE_WORKER_STARTED.swap(true, Ordering::AcqRel) {
        let worker_for_task = Arc::clone(&worker);
        kovi::tokio::spawn(async move {
            executive_save_worker(worker_for_task).await;
        });
    }
    worker.notify.notify_one();
}

async fn executive_save_worker(worker: Arc<ExecutiveSaveWorker>) {
    loop {
        worker.notify.notified().await;
        loop {
            let Some(store) = EXECUTIVE_STORE.get().cloned() else {
                break;
            };
            let Some(controller) = EXECUTIVE_CONTROLLER.get().cloned() else {
                break;
            };
            let save_lock = EXECUTIVE_SAVE_LOCK
                .get_or_init(|| Arc::new(AsyncMutex::new(())))
                .clone();
            let operation_guard = save_lock.lock().await;
            let state_lock = executive_save_state();
            let mut state = state_lock.lock().await;
            if !state.dirty || state.erasure_blocked {
                drop(operation_guard);
                break;
            }
            let epoch = state.erasure_epoch;
            let snapshot = controller.snapshot();
            let saved_version = snapshot.version;
            state.dirty = false;
            let requested_version = state.requested_version;
            drop(state);
            let result = kovi::tokio::time::timeout(
                EXECUTIVE_SAVE_TIMEOUT,
                store.save_runtime_snapshot(&snapshot),
            )
            .await;
            let current_version = controller.version();
            let mut state = state_lock.lock().await;
            let epoch_changed = epoch != state.erasure_epoch;
            let blocked = state.erasure_blocked;
            drop(operation_guard);

            match result {
                Ok(Ok(())) if !epoch_changed && !blocked => {
                    if state.dirty
                        || current_version > saved_version
                        || requested_version > saved_version
                    {
                        state.dirty = true;
                        state.requested_version = state.requested_version.max(current_version);
                    } else {
                        state.requested_version = saved_version;
                    }
                }
                Ok(Ok(())) => {
                    // An erasure cannot normally change the epoch while the
                    // operation lock is held. If a future alternate path does
                    // so, preserve any newer request/version and never mark
                    // the stale snapshot as the latest durable state.
                    if state.dirty || current_version > saved_version {
                        state.dirty = true;
                        state.requested_version = state.requested_version.max(current_version);
                    } else {
                        state.requested_version = 0;
                    }
                }
                Ok(Err(error)) => {
                    state.dirty = true;
                    state.requested_version = state.requested_version.max(current_version);
                    kovi::log::warn!("Yunxi Executive persistence failed: {error}");
                    drop(state);
                    wait_for_executive_save_retry(&worker).await;
                    continue;
                }
                Err(_) => {
                    state.dirty = true;
                    state.requested_version = state.requested_version.max(current_version);
                    kovi::log::warn!(
                        "Yunxi Executive persistence exceeded {:?}; latest state remains dirty",
                        EXECUTIVE_SAVE_TIMEOUT
                    );
                    drop(state);
                    wait_for_executive_save_retry(&worker).await;
                    continue;
                }
            }
            drop(state);
        }
    }
}

async fn wait_for_executive_save_retry(worker: &ExecutiveSaveWorker) {
    kovi::tokio::select! {
        _ = kovi::tokio::time::sleep(EXECUTIVE_SAVE_RETRY_DELAY) => {}
        _ = worker.notify.notified() => {}
    }
}

/// Erase Executive state through an explicitly selected store. Production
/// callers and isolated integration tests use the same bounded barrier while
/// supplying the store explicitly.
pub(crate) async fn erase_executive_scopes_with_store(
    scopes: &[yunxi_core::ExecutiveScope],
    require_store: bool,
    store: Option<&PostgresExecutiveStore>,
) -> Result<usize> {
    let mut ordered = scopes
        .iter()
        .map(|scope| {
            executive_store::scope_key(scope)
                .map(|key| (key, scope.clone()))
                .map_err(anyhow::Error::from)
        })
        .collect::<Result<Vec<_>>>()?;
    ordered.sort_by(|left, right| left.0.cmp(&right.0));
    ordered.dedup_by(|left, right| left.0 == right.0);
    if ordered.is_empty() {
        return Ok(0);
    }

    let save_lock = EXECUTIVE_SAVE_LOCK
        .get_or_init(|| Arc::new(AsyncMutex::new(())))
        .clone();
    let _operation_guard = save_lock.lock().await;
    let state_lock = executive_save_state();
    let mut state = state_lock.lock().await;
    let was_blocked = state.erasure_blocked;
    state.erasure_blocked = true;
    if !was_blocked {
        state.erasure_epoch = state.erasure_epoch.saturating_add(1);
        state.erasure_start_version = EXECUTIVE_CONTROLLER
            .get()
            .map_or(0, yunxi_core::ExecutiveController::version);
        // Any request made before this barrier belongs to the state that is
        // about to be erased. Requests arriving while blocked are retained.
        state.dirty = false;
        state.requested_version = 0;
    }
    drop(state);

    let result = async {
        let mut last_error = None;
        for attempt in 1..=EXECUTIVE_ERASURE_MAX_ATTEMPTS {
            match erase_executive_scopes_once(&ordered, require_store, store).await {
                Ok(removed) => return Ok(removed),
                Err(error) => {
                    kovi::log::warn!(
                        "Yunxi Executive scope erasure attempt {attempt}/{} failed: {error}",
                        EXECUTIVE_ERASURE_MAX_ATTEMPTS
                    );
                    last_error = Some(error);
                    if attempt < EXECUTIVE_ERASURE_MAX_ATTEMPTS {
                        kovi::tokio::time::sleep(
                            EXECUTIVE_ERASURE_RETRY_DELAY.saturating_mul(attempt as u32),
                        )
                        .await;
                    }
                }
            }
        }
        Err(last_error.expect("bounded Executive erasure loop always records a failed attempt"))
    }
    .await;

    match result {
        Ok(removed) => {
            let cleared_version = if ordered
                .iter()
                .any(|(_, scope)| matches!(scope, yunxi_core::ExecutiveScope::Global))
            {
                if let Some(controller) = EXECUTIVE_CONTROLLER.get() {
                    controller.clear_for_scope_data_erasure(&yunxi_core::ExecutiveScope::Global);
                    Some(controller.version())
                } else {
                    None
                }
            } else {
                if let Some(controller) = EXECUTIVE_CONTROLLER.get() {
                    for (_, scope) in &ordered {
                        controller.clear_for_scope_data_erasure(scope);
                    }
                }
                None
            };
            let current_version = EXECUTIVE_CONTROLLER
                .get()
                .map_or(0, yunxi_core::ExecutiveController::version);
            let mut state = state_lock.lock().await;
            let wake = finish_executive_erasure_state(&mut state, current_version, cleared_version);
            drop(state);
            if wake {
                wake_executive_save_worker();
            }
            Ok(removed)
        }
        Err(error) => {
            // Keep the block and epoch active. A later retry must acquire this
            // same lock and complete successfully before any save is allowed.
            Err(error)
        }
    }
}

/// Release a successful erase barrier without losing state that was produced
/// after the barrier began. `cleared_version` is the post-reset baseline for a
/// global erase; scoped erasures compare against the version at barrier start.
fn finish_executive_erasure_state(
    state: &mut ExecutiveSaveState,
    current_version: u64,
    cleared_version: Option<u64>,
) -> bool {
    let baseline = cleared_version.unwrap_or(state.erasure_start_version);
    let changed_after_barrier = current_version > baseline;
    let needs_save = state.dirty || changed_after_barrier;
    state.erasure_blocked = false;
    state.erasure_start_version = 0;
    state.dirty = needs_save;
    if needs_save {
        state.requested_version = state.requested_version.max(current_version);
    } else {
        state.requested_version = 0;
    }
    needs_save
}

async fn erase_executive_scopes_once(
    ordered: &[(String, yunxi_core::ExecutiveScope)],
    require_store: bool,
    store: Option<&PostgresExecutiveStore>,
) -> Result<usize> {
    let mut removed = 0_usize;
    if let Some(store) = store {
        for (_, scope) in ordered {
            let count = kovi::tokio::time::timeout(
                EXECUTIVE_ERASURE_TIMEOUT,
                store.erase_scope_data(scope),
            )
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "Yunxi Executive scope erasure exceeded {:?}",
                    EXECUTIVE_ERASURE_TIMEOUT
                )
            })?
            .map_err(anyhow::Error::from)?;
            removed = removed
                .checked_add(count)
                .ok_or_else(|| anyhow::anyhow!("Yunxi Executive erased row count overflow"))?;
        }
    } else if require_store {
        return Err(anyhow::anyhow!(
            "Yunxi Executive store is unavailable; erasure barrier remains closed"
        ));
    }
    Ok(removed)
}

pub(crate) fn register_mind_candidates(
    idempotency_key: String,
    context: MindCandidateContext,
    candidates: MindCandidates,
) -> bool {
    MIND_RUNTIME
        .get()
        .is_some_and(|runtime| runtime.register_candidates(idempotency_key, context, candidates))
}

pub(crate) fn observe_mind_decision(
    projection: yunxi_core::MindDecisionProjection,
    estimated_extra_tokens: usize,
) {
    if let Some(runtime) = MIND_RUNTIME.get() {
        runtime.observe_decision(projection, estimated_extra_tokens);
    }
}

pub(crate) fn register_mind_outgoing_fence(
    idempotency_key: String,
    input: &yunxi_core::PlannerInput,
    projection: yunxi_core::MindDecisionProjection,
) -> bool {
    MIND_RUNTIME
        .get()
        .is_some_and(|runtime| runtime.register_outgoing_fence(idempotency_key, input, projection))
}

pub(crate) async fn pin_mind_outgoing_fence(
    idempotency_key: &str,
) -> Option<MindDeliveryPermit<'static>> {
    let Some(runtime) = MIND_RUNTIME.get() else {
        return Some(MindDeliveryPermit::untracked());
    };
    runtime
        .pin_revalidated_outgoing_fence(idempotency_key)
        .await
}

pub(crate) fn commit_mind_candidates(idempotency_key: &str) {
    if let Some(runtime) = MIND_RUNTIME.get() {
        runtime.commit_candidates(idempotency_key);
    }
}

pub(crate) async fn mind_proactive_signals(person_id: PersonId) -> MindProactiveSignals {
    let Some(runtime) = MIND_RUNTIME.get() else {
        return MindProactiveSignals::default();
    };
    runtime
        .proactive_signals(person_id)
        .await
        .unwrap_or_else(|error| {
            kovi::log::warn!("Yunxi Mind proactive retrieval failed: {error}");
            MindProactiveSignals::default()
        })
}

pub(crate) fn mark_mind_proactive_used(reference: MindProactiveReference) {
    if let Some(runtime) = MIND_RUNTIME.get() {
        runtime.mark_proactive_used(reference);
    }
}

pub(crate) async fn pin_mind_proactive_reference(
    reference: Option<MindProactiveReference>,
) -> Option<MindDeliveryPermit<'static>> {
    let Some(reference) = reference else {
        return Some(MindDeliveryPermit::untracked());
    };
    let runtime = MIND_RUNTIME.get()?;
    runtime.pin_proactive_reference(reference).await
}

pub(crate) fn observe_mind_maintenance_tick() {
    if let Some(bridge) = CORE_BRIDGE.get() {
        bridge.observe_maintenance_tick();
    }
}

pub(crate) async fn mind_status_report() -> Result<String> {
    let store = MIND_STORE.get().context("Yunxi Mind store 尚未初始化")?;
    let runtime = MIND_RUNTIME
        .get()
        .context("Yunxi Mind runtime 尚未初始化")?;
    let stored = store.status().await.map_err(anyhow::Error::from)?;
    let metrics = runtime.metrics();
    let reasons = runtime.reasons();
    let last_reflection =
        chrono::DateTime::<chrono::Utc>::from_timestamp_millis(metrics.last_reflection_unix_ms)
            .map_or_else(|| "尚未执行".to_string(), |at| at.to_rfc3339());
    Ok(format!(
        "Yunxi Mind 状态\n模式：{:?}\n版本：{}\n持久状态：belief {}，preference {}，interest {}，open question {}，active agenda {}\n候选：registered {}，applied {}，rejected {}\n反思：total {}，failed {}，last {}，额外模型调用 0\n更新：belief {}，preference {}，interest {}，agenda {}\n决策：observed {}，shadow delta {}，active delta {}，estimated extra tokens {}\n快照：requests {}，latency last/avg/max {:.2}/{:.2}/{:.2} ms，blocked {}\n发送栅栏：registered {}，rejected {}，stale {}\n原因：disposition={:?} tags={:?}，agenda={:?}，belief={:?}，proactive={:?}\n运行：events {}，proactive uses {}，erasures {}",
        runtime.config().influence_mode(),
        stored.version,
        stored.beliefs,
        stored.preferences,
        stored.interests,
        stored.open_questions,
        stored.active_agenda,
        metrics.candidates_registered,
        metrics.candidates_applied,
        metrics.candidates_rejected,
        metrics.reflections,
        metrics.reflection_failures,
        last_reflection,
        metrics.belief_updates,
        metrics.preference_updates,
        metrics.interest_updates,
        metrics.agenda_updates,
        metrics.decision_observations,
        metrics.shadow_decision_deltas,
        metrics.active_decision_deltas,
        metrics.estimated_extra_prompt_tokens,
        metrics.snapshot_requests,
        metrics.snapshot_latency_last_micros as f64 / 1_000.0,
        if metrics.snapshot_requests == 0 {
            0.0
        } else {
            metrics.snapshot_latency_total_micros as f64
                / metrics.snapshot_requests as f64
                / 1_000.0
        },
        metrics.snapshot_latency_max_micros as f64 / 1_000.0,
        metrics.blocked_snapshots,
        metrics.outgoing_fences_registered,
        metrics.outgoing_fences_rejected,
        metrics.outgoing_fences_stale,
        reasons.last_disposition,
        reasons.last_decision_reasons,
        reasons.last_agenda_source,
        reasons.last_belief_source,
        reasons.last_proactive_kind,
        metrics.events_observed,
        metrics.proactive_uses,
        metrics.erasures,
    ))
}

/// Return a deliberately metadata-only Intrinsic report.  The runtime report
/// is already bounded at its source; this final cap protects the chat command
/// if a future engine adds another diagnostic field.
pub(crate) fn intrinsic_status_report() -> String {
    let report = intrinsic_runtime::get()
        .map(|runtime| runtime.status_report())
        .unwrap_or_else(|| {
            "Intrinsic 状态\n加载状态：尚未安装\n能力：text=false，vision=false".to_owned()
        });
    bound_status_report(report)
}

/// Render Executive state without exposing natural-language state payloads.
/// IDs, enum values, counts, and reason tags are sufficient for operations;
/// prompts, expectation patterns, goal text, and model outputs stay private.
pub(crate) fn executive_status_report() -> String {
    let Some(controller) = executive_controller() else {
        return "Yunxi Executive 状态\n加载状态：尚未安装".to_owned();
    };
    let snapshot = controller.snapshot();
    let policy = controller.policy();
    let capability = &snapshot.cognitive_capability;
    let intrinsic = intrinsic_runtime::get();
    let (queue, inferences, vision_inferences, failures, fallbacks) = intrinsic
        .as_ref()
        .map(|runtime| {
            let metrics = runtime.metrics();
            (
                format!(
                    "parallel={} timeout_ms={}",
                    runtime.runtime().config().max_parallel,
                    runtime.runtime().config().queue_timeout_ms
                ),
                metrics.inferences,
                metrics.vision_inferences,
                metrics.failures,
                metrics.fallbacks,
            )
        })
        .unwrap_or_else(|| ("unavailable".to_owned(), 0, 0, 0, 0));
    let conflict_summary = snapshot
        .active_conflicts
        .iter()
        .take(yunxi_core::MAX_SNAPSHOT_ITEMS)
        .map(|conflict| {
            format!(
                "{}:{:?}:{:.2}",
                conflict.id, conflict.kind, conflict.severity
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let goal_summary = snapshot
        .prioritized_goals
        .iter()
        .take(yunxi_core::MAX_SNAPSHOT_ITEMS)
        .map(|goal| format!("{}:{:?}:{:.2}", goal.goal_id, goal.state, goal.score))
        .collect::<Vec<_>>()
        .join(", ");
    let plan_summary = snapshot.active_plan.as_ref().map_or_else(
        || "none".to_owned(),
        |plan| {
            format!(
                "status={:?} version={} step={}/{} revisions={}",
                plan.status,
                plan.version,
                plan.current_step,
                plan.steps.len(),
                plan.revision_count
            )
        },
    );
    let decision_tags = snapshot
        .recent_decisions
        .iter()
        .flat_map(|decision| decision.reason_tags.iter())
        .take(yunxi_core::MAX_REASON_TAGS)
        .map(|tag| format!("{:?}", tag))
        .collect::<Vec<_>>()
        .join(", ");
    let reflection = mind_runtime().map_or_else(
        || "unavailable".to_owned(),
        |runtime| {
            let metrics = runtime.metrics();
            format!(
                "total={} failed={} last_unix_ms={}",
                metrics.reflections, metrics.reflection_failures, metrics.last_reflection_unix_ms
            )
        },
    );
    bound_status_report(format!(
        "Yunxi Executive 状态\n版本：{}\n当前/偏好 tier：{}/{}\nIntrinsic：health={:?} text={} vision={} version={} adapter={} manifest={}\nStrong：{}\n预算：available={:.2}/{:.2} reserve={:.2} replenishment={:.2}\n队列/指标：{}；text={} vision={} failures={} fallbacks={}\n冲突({})：{}\n目标({})：{}\n计划：{}\n期待({})：pending-only metadata\n最近决策({}) tags：{}\n反思：{}\n策略：max_plan_revisions={} candidates={} conflict_threshold={:.2} deep_reflection_budget={}",
        snapshot.version,
        capability.current_tier,
        capability.preferred_tier,
        capability.intrinsic_health,
        capability.text_available,
        capability.vision_available,
        capability
            .intrinsic_version
            .as_ref()
            .map(|version| version.model_id.as_str())
            .unwrap_or("unknown"),
        capability
            .intrinsic_version
            .as_ref()
            .and_then(|version| version.adapter_version.as_deref())
            .unwrap_or("none"),
        capability
            .intrinsic_version
            .as_ref()
            .map(|version| version.manifest_hash.as_str())
            .unwrap_or("unknown"),
        capability.strong_available,
        snapshot.attention_budget.available,
        snapshot.attention_budget.total,
        snapshot.attention_budget.reserved_for_critical,
        snapshot.attention_budget.replenishment_rate,
        queue,
        inferences,
        vision_inferences,
        failures,
        fallbacks,
        snapshot.active_conflicts.len(),
        conflict_summary,
        snapshot.prioritized_goals.len(),
        goal_summary,
        plan_summary,
        snapshot.pending_expectations.len(),
        snapshot.recent_decisions.len(),
        decision_tags,
        reflection,
        policy.max_plan_revisions,
        policy.max_candidate_count,
        policy.conflict_threshold,
        policy.deep_reflection_budget,
    ))
}

fn bound_status_report(report: String) -> String {
    const MAX_STATUS_CHARS: usize = 4_096;
    report.chars().take(MAX_STATUS_CHARS).collect()
}

/// Bootstrap canonical state from the legacy per-user profile. Existing rows
/// are Core-owned and must never be replaced by a later legacy projection;
/// both inserts therefore use an atomic `ON CONFLICT DO NOTHING` boundary.
/// The legacy bot personality remains global and is not copied into a person.
pub(crate) async fn project_legacy_user_state(
    user_id: i64,
    mood: Option<(&str, u8)>,
    relationship_level: u8,
    interaction_count: u32,
) {
    let Some(identities) = IDENTITY_STORE.get() else {
        return;
    };
    let Ok(external) = qq::person(user_id) else {
        return;
    };
    let Ok(person_id) = identities.resolve_external_identity(&external).await else {
        return;
    };
    if let Some(affect_store) = AFFECT_STORE.get()
        && let Some((mood_name, intensity)) = mood
    {
        let (valence, arousal, curiosity) = legacy_mood_projection(mood_name, intensity);
        let state = AffectState {
            valence,
            arousal,
            social_energy: (f32::from(relationship_level) / 10.0).clamp(0.0, 1.0),
            curiosity,
        };
        if let Err(error) = affect_store.seed_if_absent(person_id, state).await {
            kovi::log::warn!("Yunxi affect bootstrap failed for QQ user {user_id}: {error}");
        }
    }
    if let Some(relation_store) = RELATION_STORE.get() {
        let familiarity = (f64::from(interaction_count.min(100)) / 100.0) as f32;
        let affinity = (f32::from(relationship_level) - 5.0) / 5.0;
        let trust = (f32::from(relationship_level) - 1.0) / 9.0;
        let comfort = affinity.max(0.0);
        let tension = (-affinity).max(0.0);
        let relation = RelationState {
            person_id,
            familiarity,
            affinity: affinity.clamp(-1.0, 1.0),
            trust: trust.clamp(-1.0, 1.0),
            comfort: comfort.clamp(-1.0, 1.0),
            tension: tension.clamp(-1.0, 1.0),
        };
        if let Err(error) = relation_store.seed_if_absent(relation).await {
            kovi::log::warn!("Yunxi relation bootstrap failed for QQ user {user_id}: {error}");
        }
    }
}

fn legacy_mood_projection(mood: &str, intensity: u8) -> (f32, f32, f32) {
    let valence = match mood {
        "happy" | "excited" | "playful" | "confident" => 0.75,
        "calm" | "thoughtful" | "neutral" => 0.0,
        "sad" | "lonely" | "shy" => -0.55,
        "angry" => -0.8,
        "curious" => 0.25,
        _ => 0.0,
    };
    let arousal = ((f32::from(intensity.min(10)) / 10.0) * 2.0 - 1.0).clamp(-1.0, 1.0);
    let curiosity = if mood == "curious" { 0.9 } else { 0.5 };
    (valence, arousal, curiosity)
}

pub(crate) fn install_core_bridge(bridge: Arc<bridge::CoreBridge>) -> Result<()> {
    CORE_BRIDGE
        .set(bridge)
        .map_err(|_| anyhow::anyhow!("Yunxi CoreBridge 已经安装"))
}

pub(crate) fn install_executive_controller(
    executive: yunxi_core::ExecutiveController,
) -> Result<()> {
    EXECUTIVE_CONTROLLER
        .set(executive)
        .map_err(|_| anyhow::anyhow!("Yunxi Executive 已经安装"))
}

pub(crate) fn executive_controller() -> Option<yunxi_core::ExecutiveController> {
    EXECUTIVE_CONTROLLER.get().cloned()
}

/// Refresh startup/runtime capability facts before a new planning turn. The
/// Intrinsic self-test can finish after the bridge is installed, so the
/// Executive must not retain a stale healthy bit in its bounded snapshot.
pub(crate) fn refresh_executive_capability() {
    let (Some(controller), Some(intrinsic)) =
        (EXECUTIVE_CONTROLLER.get(), intrinsic_runtime::get())
    else {
        return;
    };
    if let Err(error) = controller.set_capability(intrinsic.capability_snapshot()) {
        kovi::log::warn!("Yunxi Executive capability refresh rejected: {error}");
    }
}

pub(crate) async fn begin_qq_user_data_erasure(user_id: i64) -> Result<bridge::UserDataErasure> {
    let bridge = CORE_BRIDGE.get().context("Yunxi CoreBridge 尚未安装")?;
    bridge.begin_user_data_erasure(user_id).await
}

pub(crate) async fn begin_qq_group_data_erasure(group_id: i64) -> Result<bridge::GroupDataErasure> {
    let bridge = CORE_BRIDGE.get().context("Yunxi CoreBridge 尚未安装")?;
    bridge.begin_group_data_erasure(group_id).await
}

pub(crate) async fn delete_mind_person_domain_data(
    person_id: Option<PersonId>,
    conversation_ids: &[ConversationId],
) -> Result<Option<MindErasureGuard>> {
    let store = MIND_STORE
        .get()
        .context("Yunxi Mind store 尚未初始化，删除屏障保持关闭")?;
    let runtime = MIND_RUNTIME
        .get()
        .context("Yunxi Mind runtime 尚未初始化，删除屏障保持关闭")?;
    let erasure = runtime.begin_erasure(person_id, conversation_ids).await;
    retry_mind_erasure(|| async {
        if let Some(person_id) = person_id {
            MindDataErasure::erase_person(store.as_ref(), person_id).await?;
        }
        for conversation_id in conversation_ids {
            MindDataErasure::erase_conversation(store.as_ref(), *conversation_id).await?;
        }
        Ok(())
    })
    .await?;
    Ok(Some(erasure))
}

pub(crate) async fn delete_mind_conversation_data(
    conversation_ids: &[ConversationId],
) -> Result<Option<MindErasureGuard>> {
    let store = MIND_STORE
        .get()
        .context("Yunxi Mind store 尚未初始化，删除屏障保持关闭")?;
    let runtime = MIND_RUNTIME
        .get()
        .context("Yunxi Mind runtime 尚未初始化，删除屏障保持关闭")?;
    let erasure = runtime.begin_erasure(None, conversation_ids).await;
    retry_mind_erasure(|| async {
        for conversation_id in conversation_ids {
            MindDataErasure::erase_conversation(store.as_ref(), *conversation_id).await?;
        }
        Ok(())
    })
    .await?;
    Ok(Some(erasure))
}

async fn retry_mind_erasure<F, Fut>(mut operation: F) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(), yunxi_core::MindDataErasureError>>,
{
    let mut last_error = None;
    for attempt in 1..=MIND_ERASURE_MAX_ATTEMPTS {
        match operation().await {
            Ok(()) => return Ok(()),
            Err(error) => {
                last_error = Some(error);
                if attempt < MIND_ERASURE_MAX_ATTEMPTS {
                    kovi::tokio::time::sleep(std::time::Duration::from_millis(
                        100 * attempt as u64,
                    ))
                    .await;
                }
            }
        }
    }
    Err(anyhow::Error::new(last_error.expect(
        "Mind erasure loop always records a failed attempt",
    )))
}

#[cfg(test)]
mod tests {
    use super::{ExecutiveSaveState, finish_executive_erasure_state};

    #[test]
    fn successful_global_erasure_keeps_post_clear_requests_dirty() {
        let mut state = ExecutiveSaveState {
            dirty: true,
            requested_version: 12,
            erasure_epoch: 1,
            erasure_blocked: true,
            erasure_start_version: 10,
        };

        assert!(finish_executive_erasure_state(&mut state, 14, Some(13)));
        assert!(!state.erasure_blocked);
        assert!(state.dirty);
        assert_eq!(state.requested_version, 14);
        assert_eq!(state.erasure_start_version, 0);
    }

    #[test]
    fn successful_scoped_erasure_drops_only_pre_barrier_state() {
        let mut state = ExecutiveSaveState {
            dirty: false,
            requested_version: 9,
            erasure_epoch: 1,
            erasure_blocked: true,
            erasure_start_version: 9,
        };

        assert!(!finish_executive_erasure_state(&mut state, 9, None));
        assert!(!state.erasure_blocked);
        assert!(!state.dirty);
        assert_eq!(state.requested_version, 0);
    }

    #[test]
    fn request_recorded_while_blocked_is_not_lost_when_version_is_unchanged() {
        let mut state = ExecutiveSaveState {
            dirty: true,
            requested_version: 9,
            erasure_epoch: 1,
            erasure_blocked: true,
            erasure_start_version: 9,
        };

        assert!(finish_executive_erasure_state(&mut state, 9, None));
        assert!(state.dirty);
        assert_eq!(state.requested_version, 9);
    }
}

/// Remove the canonical Core person and all QQ direct conversations belonging
/// to this user across bot accounts. This complements the legacy subsystem
/// deletions used by `#删除我的数据 确认`.
pub(crate) async fn delete_qq_person_domain_data(self_id: i64, user_id: i64) -> Result<u64> {
    let _route_guard = DELIVERY_ROUTE_LOCK.write().await;
    let store = IDENTITY_STORE
        .get()
        .context("Yunxi identity store 尚未初始化")?;
    let external_identity = qq::person(user_id)?;
    let direct_conversation = qq::direct(self_id, user_id)?;
    // Identity mutations fail closed for the configured owner. The cached
    // mapping may already be stale because another process changed Postgres.
    if crate::config::get().identity().owner_person_id().is_some() {
        cache_owner_route(OwnerQqRoute::Unavailable);
    }
    let deleted = store
        .delete_person_domain_data(&external_identity, &direct_conversation)
        .await
        .map_err(anyhow::Error::from);
    let _ = refresh_owner_route_while_locked().await;
    let deleted = deleted?;
    Ok(deleted.total())
}

/// Remove canonical Core group data while pinning in-process delivery routes;
/// the storage transaction also serializes cross-process commits by owner.
pub(crate) async fn delete_qq_group_domain_data(group_id: i64) -> Result<u64> {
    let _route_guard = DELIVERY_ROUTE_LOCK.write().await;
    let store = IDENTITY_STORE
        .get()
        .context("Yunxi identity store 尚未初始化")?;
    store
        .delete_qq_group_domain_data(group_id)
        .await
        .map_err(anyhow::Error::from)
}

pub(crate) async fn export_person_json(person_id: uuid::Uuid) -> Result<String> {
    let store = IDENTITY_STORE
        .get()
        .context("Yunxi identity store 尚未初始化")?;
    let export = store
        .export_person(yunxi_core::PersonId::from_uuid(person_id))
        .await
        .map_err(anyhow::Error::from)?;
    serde_json::to_string_pretty(&export).context("serialize Yunxi person export")
}

pub(crate) async fn import_person_json(payload: &str) -> Result<uuid::Uuid> {
    let _route_guard = DELIVERY_ROUTE_LOCK.write().await;
    let store = IDENTITY_STORE
        .get()
        .context("Yunxi identity store 尚未初始化")?;
    let export: identity_store::PortablePersonExport =
        serde_json::from_str(payload).context("parse Yunxi person export")?;
    let person_id = store
        .import_person(&export)
        .await
        .map_err(anyhow::Error::from)?;
    let _ = refresh_owner_route_while_locked().await;
    Ok(person_id.into_uuid())
}

pub(crate) async fn unlink_external_identity(platform: &str, external_id: &str) -> Result<bool> {
    let _route_guard = DELIVERY_ROUTE_LOCK.write().await;
    let store = IDENTITY_STORE
        .get()
        .context("Yunxi identity store 尚未初始化")?;
    let platform = yunxi_core::PlatformId::new(platform.to_owned())
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let external = yunxi_core::ExternalIdentity::new(platform, external_id.to_owned())
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    if crate::config::get().identity().owner_person_id().is_some() {
        cache_owner_route(OwnerQqRoute::Unavailable);
    }
    let unlinked = store
        .unlink_external_identity(&external)
        .await
        .map_err(anyhow::Error::from);
    let _ = refresh_owner_route_while_locked().await;
    let unlinked = unlinked?;
    Ok(unlinked)
}
