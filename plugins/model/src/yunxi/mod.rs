mod affect_store;
pub(crate) mod autonomous;
pub(crate) mod bridge;
pub(crate) mod core_model;
pub(crate) mod delivery;
mod delivery_ledger;
pub(crate) mod events;
mod executive_store;
pub(crate) mod gag_store;
mod goal_store;
mod group_cooling_store;
mod identity_store;
pub(crate) mod intrinsic_runtime;
pub(crate) mod memory_migration;
mod memory_store;
pub(crate) mod memory_writeback;
mod mind_runtime;
mod mind_store;
mod open_loop_scheduler;
mod open_loop_store;
mod owner_lock;
pub(crate) mod proactive;
pub(crate) mod qq;
mod relation_note_store;
mod relation_store;
mod schema;
pub(crate) mod turn_gate_runtime; // TurnGate completion host runtime (Phase 2)
pub(crate) mod turn_gate_shadow; // TurnGate response head shadow ledger (Phase 3)
pub(crate) mod world_model; // World Model v4 host-side runtime (shadow)
mod world_model_store; // World Model v4 persistence (infrastructure)

use affect_store::PostgresAffectStore;
use anyhow::{Context, Result};
use delivery_ledger::PostgresDeliveryLedger;
use executive_store::PostgresExecutiveStore;
use gag_store::PostgresGagStore;
use goal_store::PostgresGoalStore;
use group_cooling_store::PostgresGroupCoolingStore;
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
use relation_note_store::PostgresRelationNoteStore;
use relation_store::PostgresRelationStore;
use std::sync::{
    Arc, OnceLock, RwLock,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
use yunxi_core::{
    AffectState, ConversationId, IdentityStore, InterestId, InterestStore, MemoryStore,
    MindDataErasure, MindScope, MindSource, OpenLoopStore, PersonId, RelationState,
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
/// 相处结论（"她观察到这个人怎么对她"）的存储：可读记录，**不参与门控**。
static RELATION_NOTE_STORE: OnceLock<Arc<PostgresRelationNoteStore>> = OnceLock::new();
static GOAL_STORE: OnceLock<Arc<PostgresGoalStore>> = OnceLock::new();
static GAG_STORE: OnceLock<Arc<PostgresGagStore>> = OnceLock::new();
/// 群级降温压力（"这个群还欢迎她主动开口吗"）：只降未点名插话的频率，
/// 默认只影子观察（`silence.group_cooling_enabled = false`）。
static GROUP_COOLING_STORE: OnceLock<Arc<PostgresGroupCoolingStore>> = OnceLock::new();
/// World Model v4 persistence store (None until `world_model.enabled`).
static WORLD_MODEL_STORE: OnceLock<Arc<world_model_store::PostgresWorldModelStore>> =
    OnceLock::new();
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
        && RELATION_NOTE_STORE.get().is_some()
        && GROUP_COOLING_STORE.get().is_some()
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
    if GAG_STORE.get().is_none() {
        let store = Arc::new(PostgresGagStore::new(
            pool.clone(),
            crate::config::get().gag_ledger().clone(),
        ));
        store.initialize_schema().await?;
        let _ = GAG_STORE.set(store);
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
    // World Model v4 persistence: additive tables, only when the feature is
    // enabled (shadow/disabled deployments never touch these tables).
    if crate::config::get().world_model().enabled() && WORLD_MODEL_STORE.get().is_none() {
        let store = Arc::new(world_model_store::PostgresWorldModelStore::new(
            pool.clone(),
        ));
        store.initialize_schema().await?;
        let _ = WORLD_MODEL_STORE.set(store);
        world_model::restore_from_store().await;
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
    if RELATION_NOTE_STORE.get().is_none() {
        let store = Arc::new(PostgresRelationNoteStore::new(pool.clone()));
        store.initialize_schema().await?;
        let _ = RELATION_NOTE_STORE.set(store);
    }
    // 群级降温：证据照常记账（影子阶段也一样），只有"要不要跳过未点名抽样"
    // 由 `silence.group_cooling_enabled` 决定。
    if GROUP_COOLING_STORE.get().is_none() {
        let store = Arc::new(PostgresGroupCoolingStore::new(pool.clone()));
        store.initialize_schema().await?;
        let _ = GROUP_COOLING_STORE.set(store);
    }
    if GOAL_STORE.get().is_none() {
        let store = Arc::new(PostgresGoalStore::new(pool.clone()));
        store.initialize_schema().await?;
        let _ = GOAL_STORE.set(store);
    }
    if MIND_STORE.get().is_none() {
        let store = Arc::new(PostgresMindStore::new(pool.clone()));
        store.initialize_schema().await?;
        store.ensure_self_model().await?;
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

pub(crate) fn gag_store() -> Option<Arc<PostgresGagStore>> {
    GAG_STORE.get().cloned()
}

pub(crate) fn world_model_store() -> Option<Arc<world_model_store::PostgresWorldModelStore>> {
    WORLD_MODEL_STORE.get().cloned()
}

#[allow(dead_code)]
pub(crate) fn memory_store() -> Option<Arc<PostgresMemoryStore>> {
    MEMORY_STORE.get().cloned()
}

/// Record a bounded, durable "world fact" about the owner-world (a project, a
/// task, a build state, a recurring thing) as a retrievable memory, so the core
/// learns about your life the same way it learns from chat. When `watch` is
/// true it also creates a bounded, low-salience follow-up open loop so the
/// proactive system may later surface it ("your build recovered"). When
/// `interest_topic` is provided it additionally seeds/boosts a bounded Mind
/// interest about that subject, so she genuinely "cares" about it and it can
/// influence her interests/topic/proactive behaviour — not just sit as passive
/// memory. The summary is bounded and importance is clamped. Returns the new
/// memory id.
#[allow(dead_code)]
pub(crate) async fn observe_world_fact(
    scope: yunxi_core::MemoryScope,
    summary: &str,
    importance: u8,
    watch: bool,
    interest_topic: Option<&str>,
) -> anyhow::Result<yunxi_core::MemoryId> {
    let Some(store) = memory_store() else {
        anyhow::bail!("memory store is unavailable");
    };
    let draft = yunxi_core::world_fact_draft(scope, summary, importance, chrono::Utc::now())?;
    let memory = store.remember(&draft).await?;
    if watch && let Some(open_loops) = open_loop_store() {
        let owner = match scope {
            yunxi_core::MemoryScope::Person(person_id) => {
                yunxi_core::OpenLoopOwner::Person(person_id)
            }
            yunxi_core::MemoryScope::Conversation(conversation_id) => {
                yunxi_core::OpenLoopOwner::Conversation(conversation_id)
            }
            yunxi_core::MemoryScope::Global => yunxi_core::OpenLoopOwner::Global,
        };
        let now = chrono::Utc::now();
        let open_loop = yunxi_core::world_loop_draft(
            owner,
            summary,
            40,
            Some(now),
            &format!("world:{}", memory.id()),
        )?
        .with_expires_at(Some(now + chrono::Duration::days(1)));
        open_loops.create(&open_loop).await?;
    }
    if let Some(topic) = interest_topic {
        seed_world_interest(topic).await?;
    }
    // Shadow-mode World Model: record the same fact as a structured
    // observation so the v4 runtime can learn (v4 §85–86). No-op when the
    // feature is disabled; never blocks the caller.
    world_model::record_observation(
        match scope {
            yunxi_core::MemoryScope::Global => yunxi_core::WorldScope::Global,
            yunxi_core::MemoryScope::Person(person_id) => {
                yunxi_core::WorldScope::Person { person_id }
            }
            yunxi_core::MemoryScope::Conversation(conversation_id) => {
                yunxi_core::WorldScope::Conversation { conversation_id }
            }
        },
        yunxi_core::world_model::ObservationKind::SystemState,
        yunxi_core::world_model::ObservationSource::SystemState,
        summary,
        None,
    );
    Ok(memory.id())
}

/// Upper bound on distinct world interests the core will hold, so a long series
/// of world facts cannot grow the interest set without a cap.
const MAX_WORLD_INTERESTS: usize = 32;

/// Seed or boost a bounded Mind interest for a watched world subject. Existing
/// interest (by normalized topic key) is nudged up; otherwise a new bounded
/// interest is created only while under the capacity cap. Best-effort: a store
/// or validation failure never fails the outer world-fact recording.
async fn seed_world_interest(topic: &str) -> anyhow::Result<()> {
    let Some(store) = mind_store() else {
        return Ok(());
    };
    let now = chrono::Utc::now();
    let draft = yunxi_core::Interest::new(
        InterestId::new(),
        topic,
        0.25,
        0.05,
        0.6,
        MindSource::ToolResult,
        now,
    )?;
    let topic_key = draft.topic_key().to_owned();
    if let Some(existing) = store.find_by_key(&topic_key).await? {
        let activated = existing.activate(0.15, 0.05, 0.6, now)?;
        store.put(&activated, Some(existing.version())).await?;
    } else if store.relevant("", MAX_WORLD_INTERESTS).await?.len() < MAX_WORLD_INTERESTS {
        store.put(&draft, None).await?;
    }
    Ok(())
}

#[allow(dead_code)]
pub(crate) fn affect_store() -> Option<Arc<PostgresAffectStore>> {
    AFFECT_STORE.get().cloned()
}

#[allow(dead_code)]
pub(crate) fn relation_store() -> Option<Arc<PostgresRelationStore>> {
    RELATION_STORE.get().cloned()
}

/// 相处结论的存储出口。
///
/// 与 `relation_store()` 同一套注册方式。没初始化时调用方按"暂时落不了库"处理就
/// 行——写入是 fail-soft 的，只记日志，绝不拖垮反思（见
/// `MindRuntime::persist_relation_notes`）。这些记录**不参与硬门控**。
pub(crate) fn relation_note_store() -> Option<Arc<PostgresRelationNoteStore>> {
    RELATION_NOTE_STORE.get().cloned()
}

#[allow(dead_code)]
pub(crate) fn goal_store() -> Option<Arc<PostgresGoalStore>> {
    GOAL_STORE.get().cloned()
}

/// 群级降温压力的存储出口。
///
/// 与 `relation_store()` 同一套注册方式。没初始化时调用方按"还不知道这个群
/// 冷不冷"处理——抽样照常放行：这是降频通道，失败方向必须是允许。
pub(crate) fn group_cooling_store() -> Option<Arc<PostgresGroupCoolingStore>> {
    GROUP_COOLING_STORE.get().cloned()
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

pub(crate) fn discard_mind_outgoing_fence(idempotency_key: &str) {
    if let Some(runtime) = MIND_RUNTIME.get() {
        runtime.discard_outgoing_fence(idempotency_key);
    }
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

/// `#立场` 的内容：她目前持有哪些看法，以及每条有多稳固。
///
/// 存在的理由有两条：一是**验收**——立场层整条管道曾经静默产出 0 行，没有这个命令
/// 就只能去查数据库表；二是这件事本身就值得她能"说"出来。
pub(crate) async fn stances_report() -> Result<String> {
    let store = MIND_STORE.get().context("Yunxi Mind store 尚未初始化")?;
    let runtime = MIND_RUNTIME
        .get()
        .context("Yunxi Mind runtime 尚未初始化")?;
    let capacity = runtime.config().max_learned_beliefs_per_scope();
    // 全限定调用：BeliefStore 与 InterestStore 等方法同名，引 trait 会让这个文件里
    // 所有 store 调用都变成二义。
    let beliefs = yunxi_core::BeliefStore::relevant(
        store.as_ref(),
        &[MindScope::Global],
        "",
        chrono::Utc::now(),
        capacity,
    )
    .await
    .map_err(anyhow::Error::from)?;
    let mut report = format!("芸汐的立场（{} 条，上限 {capacity}）", beliefs.len());
    if beliefs.is_empty() {
        report.push_str(
            "\n她还没有形成任何立场。立场候选来自模型回复里带的 mind_candidates；\n\
             如果这里长期为空，先查日志里有没有「立场候选被安全过滤丢弃」。",
        );
        return Ok(report);
    }
    for (index, belief) in beliefs.iter().enumerate() {
        report.push_str(&format!(
            "\n{}. {}\n   置信 {:.2} · 稳固 {:.2} · 被挑战 {} 次 · 证据 {} 条 · 更新 {}",
            index + 1,
            belief.proposition(),
            belief.confidence(),
            belief.stability(),
            belief.contradiction_count(),
            belief.evidence_refs().len(),
            belief.updated_at().format("%Y-%m-%d %H:%M UTC"),
        ));
    }
    Ok(report)
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
        "Yunxi Executive 状态\n版本：{}\n当前/偏好 tier：{}/{}\nIntrinsic：health={:?} text={} vision={} version={} adapter={} manifest={}\nStrong：{}\n预算：available={:.2}/{:.2} reserve={:.2} replenishment={:.2}\n队列/指标：{}；text={} vision={} failures={} fallbacks={}\n冲突({})：{}\n目标({})：{}\n计划：{}\n期待({})：pending-only metadata\n最近决策({}) tags：{}\n反思：{}\n策略：max_plan_revisions={} conflict_threshold={:.2}",
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
        policy.conflict_threshold,
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
        let relation = legacy_relation_projection(person_id, relationship_level, interaction_count);
        if let Err(error) = relation_store.seed_if_absent(relation).await {
            kovi::log::warn!("Yunxi relation bootstrap failed for QQ user {user_id}: {error}");
        }
    }
}

/// 把 legacy 档案的两个数字投影成 Core 的关系初值。
///
/// 抽成纯函数是为了让两条不变量**可测**：
///
/// 1. **投影不许凭空造出张力**：`tension` 的语义是"被持续不友好对待的证据累积"，
///    只有证据通道能抬升它，legacy 档案里根本没有这个信息。
/// 2. **投影不许凭空造出负好感**：等级低是"不熟"，不是"有仇"。这里曾经写
///    `affinity = (level - 5) / 5`，于是 `level = 1`（legacy 里是"礼貌、稍微正式"，
///    也是新用户的默认值，线上八成档案都是它）被译成好感 **-0.8**。在好感只是
///    后台一个数字的年代它看起来只是"偏低"；好感接进语气档与放行判据之后，它意味着
///    "每个新认识的人一建档就是她不喜欢的人"。等级 1..=4 一律给 0（中性起步），
///    5..=10 才线性给到 0.6——"不熟"与"不喜欢"必须落在刻度两边。
fn legacy_relation_projection(
    person_id: PersonId,
    relationship_level: u8,
    interaction_count: u32,
) -> RelationState {
    let familiarity = (f64::from(interaction_count.min(100)) / 100.0) as f32;
    let level = f32::from(relationship_level);
    // 1..=4（礼貌/正式）→ 0；5..=10 → 0.1..=0.6。上界刻意不冲到 1.0：
    // 建档只是"以前聊得不错"，真正的好感要靠相处证据自己挣。
    let affinity = if level <= 4.0 {
        0.0
    } else {
        (level - 4.0) / 10.0
    };
    let trust = (level - 1.0) / 9.0;
    let comfort = affinity.max(0.0);
    RelationState {
        person_id,
        familiarity: familiarity.clamp(-1.0, 1.0),
        affinity: affinity.clamp(-1.0, 1.0),
        trust: trust.clamp(-1.0, 1.0),
        comfort: comfort.clamp(-1.0, 1.0),
        // 恒为 0，不从 `-affinity` 反推。这里曾经写 `(-affinity).max(0.0)`，于是
        // `relationship_level = 1` 被译成张力 0.8。在静默门控用 0.6 当阈值之前
        // 那个值只是语气提示，无害；门控上线后它意味着"每个新认识的人一建档就带着
        // 越线的张力，第一条 @ 她的话就被判不接"。等级低是"不熟"，不是"有仇"，
        // 这两件事不能共用一根刻度。
        tension: 0.0,
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
    // 账本条目按 QQ 号原文存（`GagScope::Person(user_id.to_string())`），同样不在这套
    // 外键级联里，必须显式删；同一个人可能换过号，所以别名一起清。
    // **必须在身份行被删掉之前读**：`delete_person_domain_data` 会把映射一并删掉。
    let gag_user_ids = match store.qq_person_domain_targets(user_id).await {
        Ok(targets) if !targets.qq_user_ids.is_empty() => targets.qq_user_ids,
        Ok(_) => vec![user_id],
        Err(error) => {
            eprintln!("[WARN] 读取 QQ 别名失败，账本只按当前号清理 (用户: {user_id}): {error}");
            vec![user_id]
        }
    };
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
    // 相处结论按"显示名/QQ 的文本"存，不在上面那套外键级联里，必须显式删。
    // 只用无歧义标识（QQ 号 + 外部身份）：显示名会撞名，传进去等于全局删别人的。
    delete_relation_notes_for_person(user_id, external_identity.external_id()).await;
    if let Some(gag_store) = gag_store() {
        delete_gag_entries_for_person(&gag_store, &gag_user_ids, user_id).await;
    }
    Ok(deleted.total())
}

/// 清掉某个人的账本条目（`GagScope::Person` 的 key 就是 QQ 号原文，按号逐个清）。
///
/// 这些行是用户自己口述的约定/芥蒂原文，会被注入回复上下文，所以和相处结论一样属于
/// "必须显式删、不能被外键级联覆盖"的一类。不删的后果不是少一行数据：回执告诉用户
/// "你的可归属数据已删除"，而 `#账本` 里还列着，换个身份映射回来还会继续进上下文。
///
/// 失败只记日志、不阻断擦除主流程，与 `delete_relation_notes_for_person` 同一约定：
/// 擦除本身已经完成，一条附加清理失败不该让用户看到"删除失败"，但必须留下可查的痕迹。
async fn delete_gag_entries_for_person(
    store: &PostgresGagStore,
    user_ids: &[i64],
    user_id: i64,
) -> u64 {
    let mut keys: Vec<String> = user_ids.iter().map(|id| id.to_string()).collect();
    keys.sort();
    keys.dedup();
    let mut deleted = 0_u64;
    for key in keys {
        match store
            .delete_for_scope(gag_store::GagScope::Person(key.clone()))
            .await
        {
            Ok(rows) => deleted = deleted.saturating_add(rows),
            Err(error) => eprintln!("[WARN] 删除账本条目失败 (scope: {key}): {error}"),
        }
    }
    if deleted > 0 {
        println!("[INFO] 已删除账本条目 {deleted} 条 (用户: {user_id})");
    }
    deleted
}

/// 清掉某个 QQ 用户相关的相处结论（只按 QQ 号原文与外部身份这两个**无歧义**标识）。
///
/// 失败只记日志、不阻断擦除主流程：擦除本身已经完成，一条附加清理失败不该
/// 让用户看到"删除失败"——但必须留下可查的痕迹。
async fn delete_relation_notes_for_person(user_id: i64, external_identity: &str) {
    let Some(store) = relation_note_store() else {
        return;
    };
    let keys = relation_note_erasure_keys(user_id, external_identity);
    match store.delete_targets(&keys).await {
        Ok(deleted) if deleted > 0 => {
            println!("[INFO] 已删除相处结论 {deleted} 条 (用户: {user_id})");
        }
        Ok(_) => {}
        Err(error) => eprintln!("[WARN] 删除相处结论失败 (用户: {user_id}): {error}"),
    }
}

/// 按人擦除相处结论时用哪些键：**只有无歧义标识**（QQ 号原文与外部身份）。
///
/// 刻意**不**包含昵称/群名片。这张表按文本存、`delete_targets` 又没有作用域谓词，
/// 所以传进去的每一个键都是"全局删"。昵称是有歧义的：把昵称改成另一个成员的名字
/// 再执行 `#删除我的数据`，就会删掉无关群里关于那位真实成员的结论——那是删别人的
/// 数据。一条有歧义的键不足以支撑一次删除，所以宁可少删（模型用昵称写下的结论会
/// 留下，模块文档本来就把"别名覆盖做不到穷尽"列为既定代价），也不误删他人。
///
/// 顺带说明为什么不能用"限定作用域"来折中：结论的 scope 来自反思输入的 `MindScope`
/// （`mind_runtime.rs:2410`），关于某人的结论可以写在**任何**会话作用域里（他在哪个
/// 群说过话，那个群的作用域就可能有），从这个人自己的身份出发枚举不出来；按作用域
/// 限定会把群里的结论漏掉，那是删不干净他自己的数据。
///
/// 群级擦除不受影响：`delete_qq_group_domain_data` 走 `delete_conversations`，本来就
/// 是按会话作用域删的。
fn relation_note_erasure_keys(user_id: i64, external_identity: &str) -> Vec<String> {
    vec![user_id.to_string(), external_identity.to_string()]
}

/// 解析某个 QQ 群在 Core 里的会话 id（相处结论与群级压力都按会话作用域存）。
///
/// 解析失败不算错误：群数据擦除本身已经完成，附加清理拿不到 id 时只记日志。
async fn conversation_ids_for_group(group_id: i64) -> Option<Vec<uuid::Uuid>> {
    let store = IDENTITY_STORE.get()?;
    let external = qq::group(group_id).ok()?;
    match store.resolve_conversation(&external).await {
        Ok(conversation_id) => Some(vec![conversation_id.into_uuid()]),
        Err(error) => {
            eprintln!("[WARN] 解析群会话 id 失败，相处结论未清理 (群组: {group_id}): {error}");
            None
        }
    }
}

/// Remove canonical Core group data while pinning in-process delivery routes;
/// the storage transaction also serializes cross-process commits by owner.
pub(crate) async fn delete_qq_group_domain_data(group_id: i64) -> Result<u64> {
    let _route_guard = DELIVERY_ROUTE_LOCK.write().await;
    let store = IDENTITY_STORE
        .get()
        .context("Yunxi identity store 尚未初始化")?;
    let deleted = store
        .delete_qq_group_domain_data(group_id)
        .await
        .map_err(anyhow::Error::from)?;
    // 群级相处结论按会话作用域存，不在上面那套外键级联里，必须显式删：
    // 否则同一个群被重建、或群号被复用时会继承上一个群的判断。
    if let Some(notes) = relation_note_store()
        && let Some(conversations) = conversation_ids_for_group(group_id).await
        && !conversations.is_empty()
    {
        match notes.delete_conversations(&conversations).await {
            Ok(deleted) if deleted > 0 => {
                println!("[INFO] 已删除本群相处结论 {deleted} 条 (群组: {group_id})");
            }
            Ok(_) => {}
            Err(error) => eprintln!("[WARN] 删除本群相处结论失败 (群组: {group_id}): {error}"),
        }
    }
    Ok(deleted)
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

#[cfg(test)]
mod tests {
    use super::{ExecutiveSaveState, finish_executive_erasure_state, legacy_relation_projection};
    use yunxi_core::PersonId;

    #[test]
    fn legacy_projection_never_invents_relation_tension() {
        // 每个等级都不许凭空造出张力。这条不变量是有代价的：静默门控拿 0.6 当
        // 阈值，而等级 1 是新用户默认值——投影一旦给出张力，等于"新人第一条
        // @ 她的话就不回"。（2026-09-14 线上事故：等级 1 → 0.8 → 被静默。）
        for level in 1..=10u8 {
            let relation = legacy_relation_projection(PersonId::new(), level, 20);
            assert_eq!(relation.tension, 0.0, "等级 {level} 不该投影出张力");
        }
    }

    #[test]
    fn legacy_projection_still_carries_familiarity_and_warmth_dimensions() {
        // 亲密度与信任照旧按等级投影，"熟不熟"仍然进得来。
        let stranger = legacy_relation_projection(PersonId::new(), 1, 100);
        let intimate = legacy_relation_projection(PersonId::new(), 10, 100);
        assert_eq!(stranger.familiarity, 1.0, "互动次数应当折算成熟悉度");
        assert_eq!(stranger.comfort, 0.0, "生分没有舒适度可言，但也不是负的");
        assert!(intimate.affinity > 0.0);
        assert!(intimate.trust > stranger.trust);
    }

    #[test]
    fn legacy_projection_never_invents_dislike() {
        // 等级低是"不熟"，不是"有仇"：好感接进语气档与未点名放行之后，
        // 一个负的初值等于"每个新认识的人一建档就是她不喜欢的人"
        // （线上八成档案是新用户默认等级 1）。
        for level in 1..=4u8 {
            let relation = legacy_relation_projection(PersonId::new(), level, 0);
            assert_eq!(
                relation.affinity, 0.0,
                "等级 {level}（礼貌/正式）应当是中性起步，不是负好感"
            );
        }
        // 高等级仍然带得出"以前就处得不错"，但不冲到满值——好感要靠证据挣。
        let intimate = legacy_relation_projection(PersonId::new(), 10, 0);
        assert!(
            intimate.affinity > 0.0 && intimate.affinity < 1.0,
            "等级 10 应当是正向但不满值：{}",
            intimate.affinity
        );
        let friendly = legacy_relation_projection(PersonId::new(), 7, 0);
        assert!(friendly.affinity > 0.0 && friendly.affinity < intimate.affinity);
    }

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

#[cfg(test)]
mod erasure_tests {
    use super::{delete_gag_entries_for_person, relation_note_erasure_keys};
    use crate::yunxi::identity_store::PostgresIdentityStore;
    use sqlx_postgres::PgPool;
    use std::sync::Arc;

    /// 擦除会一路删到 memories / open-loops / goals / affect / relations / 世界模型，
    /// 测试库必须先把这些 schema 建齐。缺任何一张表都会在真正要验的那一步之前先报
    /// 42P01，让用例以错误的理由变红（这个坑踩过一次）。
    async fn initialize_erasure_schemas(pool: &PgPool) -> Arc<PostgresIdentityStore> {
        let store = Arc::new(PostgresIdentityStore::new(pool.clone()));
        store
            .initialize_schema()
            .await
            .expect("应初始化身份 schema");
        crate::yunxi::memory_store::PostgresMemoryStore::new(
            Arc::clone(&crate::memory::MEMORY_MANAGER),
            Arc::clone(&store),
            pool.clone(),
        )
        .initialize_schema()
        .await
        .expect("应初始化 memory schema");
        crate::yunxi::delivery_ledger::PostgresDeliveryLedger::new(pool.clone())
            .initialize_schema()
            .await
            .expect("应初始化 delivery ledger schema");
        crate::yunxi::open_loop_store::PostgresOpenLoopStore::new(pool.clone())
            .initialize_schema()
            .await
            .expect("应初始化 open-loop schema");
        crate::yunxi::goal_store::PostgresGoalStore::new(pool.clone())
            .initialize_schema()
            .await
            .expect("应初始化 goal schema");
        crate::yunxi::affect_store::PostgresAffectStore::new(pool.clone())
            .initialize_schema()
            .await
            .expect("应初始化 affect schema");
        crate::yunxi::relation_store::PostgresRelationStore::new(pool.clone())
            .initialize_schema()
            .await
            .expect("应初始化 relation schema");
        store
    }

    /// 造一个"有待删数据"的人：身份 + 私聊会话 + 一条属于他的记忆，并返回句柄。
    async fn seed_person_with_memory(
        pool: &PgPool,
        store: &Arc<PostgresIdentityStore>,
    ) -> (
        yunxi_core::ExternalIdentity,
        yunxi_core::ExternalConversation,
        uuid::Uuid,
    ) {
        use sqlx_core::query::query;

        let suffix = (uuid::Uuid::new_v4().as_u128() % 1_000_000_000) as i64;
        let user_id = 1_000_000_000_000_i64 + suffix;
        let identity = super::qq::person(user_id).expect("valid identity");
        let direct = super::qq::direct(9_000_000_000_000_i64 + suffix, user_id)
            .expect("valid direct conversation");
        let person_id = store
            .resolve_identity(&identity)
            .await
            .expect("identity should resolve");
        store
            .resolve_direct_for_person(person_id, &direct)
            .await
            .expect("direct conversation should resolve");
        let person_uuid = person_id.into_uuid();
        query(
            "INSERT INTO yunxi_memories
                (id, scope_kind, scope_id, kind, content, importance, tags, occurred_at)
             VALUES ($1, 'person', $2, 'fact', 'deletion test', 50, '[]', NOW())",
        )
        .bind(uuid::Uuid::new_v4())
        .bind(person_uuid)
        .execute(pool)
        .await
        .expect("应创建测试记忆");
        (identity, direct, person_uuid)
    }

    /// 账本条目按 QQ 号原文存、也被注入回复上下文，所以必须显式删干净：不只是
    /// 当前号，同一个人换过的号（别名）也要清；同时不能碰到别人的条目。
    #[test]
    #[ignore = "requires PostgreSQL via DATABASE_URL"]
    fn person_erasure_purges_the_gag_ledger_for_every_alias() {
        use crate::yunxi::gag_store::{GagKind, GagScope, PostgresGagStore};
        use sqlx_postgres::PgPoolOptions;

        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let database_url = std::env::var("DATABASE_URL").expect("需要 DATABASE_URL");
                let pool = PgPoolOptions::new()
                    .max_connections(4)
                    .connect(&database_url)
                    .await
                    .expect("应连接 PostgreSQL");
                let store =
                    PostgresGagStore::new(pool.clone(), crate::config::get().gag_ledger().clone());
                store
                    .initialize_schema()
                    .await
                    .expect("应初始化账本 schema");

                let suffix = (uuid::Uuid::new_v4().as_u128() % 1_000_000_000) as i64;
                let primary = 1_000_000_000_000_i64 + suffix;
                let alias = 1_500_000_000_000_i64 + suffix;
                let stranger = 2_000_000_000_000_i64 + suffix;
                for (scope_id, text) in [
                    (primary, "答应过要早睡"),
                    (alias, "换号之前答应的事"),
                    (stranger, "别人的约定"),
                ] {
                    store
                        .add(
                            GagScope::Person(scope_id.to_string()),
                            GagKind::Promise,
                            text,
                            60,
                        )
                        .await
                        .expect("应写入账本条目");
                }

                let deleted =
                    delete_gag_entries_for_person(&store, &[primary, alias], primary).await;
                assert_eq!(deleted, 2, "当前号与别名都该被清掉");
                assert!(
                    store
                        .list_open(GagScope::Person(primary.to_string()), 5)
                        .await
                        .expect("应可读回")
                        .is_empty()
                );
                assert!(
                    store
                        .list_open(GagScope::Person(alias.to_string()), 5)
                        .await
                        .expect("应可读回")
                        .is_empty()
                );
                assert_eq!(
                    store
                        .list_open(GagScope::Person(stranger.to_string()), 5)
                        .await
                        .expect("应可读回")
                        .len(),
                    1,
                    "不能顺手删掉别人的账本"
                );
            });
    }

    /// 短 id 前缀查找走的是 UUID 范围比较（`id >= lower AND id < upper`），不是
    /// `CAST(id AS TEXT) LIKE`：后者既用不上主键索引，又会让输入里的 `_`/`%` 变成
    /// 通配符——一次"前缀歧义"会被报成"没找到这条账"。
    #[test]
    #[ignore = "requires PostgreSQL via DATABASE_URL"]
    fn gag_prefix_lookup_uses_a_uuid_range_and_rejects_wildcards() {
        use crate::yunxi::gag_store::{GagKind, GagScope, PostgresGagStore};
        use sqlx_postgres::PgPoolOptions;

        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let database_url = std::env::var("DATABASE_URL").expect("需要 DATABASE_URL");
                let pool = PgPoolOptions::new()
                    .max_connections(4)
                    .connect(&database_url)
                    .await
                    .expect("应连接 PostgreSQL");
                let store =
                    PostgresGagStore::new(pool.clone(), crate::config::get().gag_ledger().clone());
                store
                    .initialize_schema()
                    .await
                    .expect("应初始化账本 schema");

                let suffix = (uuid::Uuid::new_v4().as_u128() % 1_000_000_000) as i64;
                let scope = GagScope::Person(format!("{}", 3_000_000_000_000_i64 + suffix));
                let first = store
                    .add(scope.clone(), GagKind::Promise, "答应过要早睡", 60)
                    .await
                    .expect("应写入第一条");
                store
                    .add(scope.clone(), GagKind::Promise, "答应过要带伞", 60)
                    .await
                    .expect("应写入第二条");

                let prefix = &first.to_string()[..8];
                assert_eq!(
                    store
                        .fulfill_by_prefix(prefix)
                        .await
                        .expect("前缀查找应成功"),
                    Some(first),
                    "八位前缀应当唯一命中，并且真的把它标成已了结"
                );
                // 已经了结的条目不再被前缀命中。
                assert_eq!(
                    store
                        .fulfill_by_prefix(prefix)
                        .await
                        .expect("前缀查找应成功"),
                    None,
                    "已了结的条目不参与前缀匹配"
                );
                // 通配符不是十六进制，按"没找到"处理，而不是匹配到一堆行。
                assert_eq!(
                    store.fulfill_by_prefix("_").await.expect("前缀查找应成功"),
                    None
                );
                assert_eq!(
                    store.fulfill_by_prefix("%").await.expect("前缀查找应成功"),
                    None
                );
            });
    }

    async fn person_memory_rows(pool: &PgPool, person: uuid::Uuid) -> i64 {
        sqlx_core::query_scalar::query_scalar(
            "SELECT COUNT(*) FROM yunxi_memories \
             WHERE scope_kind = 'person' AND scope_id = $1",
        )
        .bind(person)
        .fetch_one(pool)
        .await
        .expect("应统计记忆行")
    }

    /// 昵称撞名不得误删他人：某人把昵称改成另一个成员的名字，再执行 `#删除我的数据`，
    /// 无关群里关于那位真实成员的相处结论必须原样留着。
    ///
    /// 这张表按文本存、`delete_targets` 又没有作用域谓词，所以"把昵称也传进去"就等于
    /// 全局删——这条测试同时钉住"按人的擦除只用无歧义标识"这个决定。
    #[test]
    #[ignore = "requires PostgreSQL via DATABASE_URL"]
    fn erasure_never_deletes_another_persons_relation_notes_by_nickname() {
        use crate::yunxi::relation_note_store::{PostgresRelationNoteStore, RelationNoteDraft};
        use sqlx_postgres::PgPoolOptions;
        use yunxi_core::EventId;
        use yunxi_core::mind::MindScope;

        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let database_url = std::env::var("DATABASE_URL").expect("需要 DATABASE_URL");
                let pool = PgPoolOptions::new()
                    .max_connections(4)
                    .connect(&database_url)
                    .await
                    .expect("应连接 PostgreSQL");
                let store = PostgresRelationNoteStore::new(pool.clone());
                store
                    .initialize_schema()
                    .await
                    .expect("应初始化相处结论 schema");

                let now = chrono::Utc::now();
                let scope = MindScope::Conversation {
                    conversation_id: yunxi_core::ConversationId::new(),
                };
                // 每次用不同的标签：这张表是持久库，写死"小明"会让第二次跑断言
                // 到上一次留下的行（本地连跑两次就会红）。仓库里其他 PG 用例同样
                // 用随机后缀。
                let suffix = (uuid::Uuid::new_v4().as_u128() % 1_000_000_000) as i64;
                // 请求者把昵称改成了别人的名字；真正属于他的行是按 QQ 号存的。
                let victim_label = format!("撞名对象{suffix}");
                let requester_label = format!("1000000000{suffix:03}");
                for (target, note) in [
                    (victim_label.as_str(), "他其实只是着急"),
                    (requester_label.as_str(), "他答应过要早睡"),
                ] {
                    let draft =
                        RelationNoteDraft::new(scope, target, note, 100, Some(EventId::new()), now)
                            .expect("测试用的相处结论应当有效");
                    store.upsert_notes(&[draft]).await.expect("应写入相处结论");
                }

                let keys = relation_note_erasure_keys(1_000_000_000_123, &requester_label);
                let deleted = store.delete_targets(&keys).await.expect("应删除");
                assert_eq!(deleted, 1, "只该删掉按 QQ 号存的那一条");

                let remaining = |target: String| {
                    let pool = pool.clone();
                    async move {
                        sqlx_core::query_scalar::query_scalar::<_, i64>(
                            "SELECT COUNT(*) FROM yunxi_relation_notes WHERE target_key = $1",
                        )
                        .bind(target)
                        .fetch_one(&pool)
                        .await
                        .expect("应统计相处结论")
                    }
                };
                assert_eq!(
                    remaining(requester_label.clone()).await,
                    0,
                    "按 QQ 号存的自己的结论应当删掉"
                );
                assert_eq!(
                    remaining(victim_label.clone()).await,
                    1,
                    "撞名不得删掉他人（真实那位同名成员）的结论"
                );
            });
    }

    /// 按人擦除相处结论只用**无歧义**标识。
    ///
    /// 这张表按文本存、`delete_targets` 没有作用域谓词，所以传进去的每个键都是全局删。
    /// 昵称有歧义（谁都能把昵称改成别人的名字），一旦传进去就会删掉无关群里关于那位
    /// 真实成员的结论——删别人的数据。宁可少删，也不误删他人。
    #[test]
    fn person_erasure_only_uses_unambiguous_relation_note_keys() {
        let keys = relation_note_erasure_keys(2515950976, "2515950976");
        assert!(keys.contains(&"2515950976".to_string()));
        assert_eq!(keys.len(), 2);

        // 关键断言：昵称**不在**键里。回归到"把显示名也传进去"会让这条失败。
        let nickname: &str = "白浅";
        assert!(
            !keys.contains(&nickname.to_string()),
            "昵称是歧义键，不能参与全局删除"
        );

        // 外站身份照旧参与（它同样唯一指向一个人）。
        let cross_platform = relation_note_erasure_keys(42, "matrix:@someone:example.org");
        assert!(cross_platform.contains(&"matrix:@someone:example.org".to_string()));
    }

    /// 数据擦除的端到端不变量：擦除之后，**下一次持久化**不能把被删的人写回来。
    ///
    /// 走的是生产路径：内存里记录观察 → `persist_if_dirty()` 落盘 → 真正的
    /// `delete_person_domain_data` → 再一次 `persist_if_dirty()`。持久化是"按内存
    /// 快照整表重写"，所以只要擦除没有让内存态重新对齐，第二次持久化就会把行写回。
    /// 这就是 `purge_world_model_domain` 必须在提交后调 `restore_from_store()` 的原因。
    ///
    /// 用 `--ignored --exact` 单进程单用例跑：它会临时替换全局配置（world_model 打开）
    /// 并占用 `WORLD_MODEL_STORE`，结尾会把原配置装回去。
    #[test]
    #[ignore = "requires PostgreSQL via DATABASE_URL"]
    fn erasure_then_persist_does_not_resurrect_the_deleted_person() {
        use crate::yunxi::world_model_store::PostgresWorldModelStore;
        use sqlx_postgres::PgPoolOptions;
        use std::sync::Arc;
        use yunxi_core::world_model::{ObservationKind, ObservationSource, WorldScope};

        // 世界模型要打开才会走 with_world / persist / restore。
        let previous_config = crate::config::get();
        let world_config =
            crate::config::validate_candidate("[world_model]\nenabled = true\npersist = true\n")
                .expect("候选配置应合法");
        crate::config::install(world_config).expect("应安装测试配置");
        super::world_model::reset_for_tests();

        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let database_url = std::env::var("DATABASE_URL").expect("需要 DATABASE_URL");
                let pool = PgPoolOptions::new()
                    .max_connections(4)
                    .connect(&database_url)
                    .await
                    .expect("应连接 PostgreSQL");
                let store = initialize_erasure_schemas(&pool).await;

                let world_store = Arc::new(PostgresWorldModelStore::new(pool.clone()));
                world_store
                    .initialize_schema()
                    .await
                    .expect("应初始化 world schema");
                let _ = super::WORLD_MODEL_STORE.set(Arc::clone(&world_store));

                let (identity, direct, person_uuid) = seed_person_with_memory(&pool, &store).await;

                // 内存里记一条这个人的观察，并落盘。
                super::world_model::record_observation(
                    WorldScope::Person {
                        person_id: yunxi_core::PersonId::from_uuid(person_uuid),
                    },
                    ObservationKind::MessageReceived,
                    ObservationSource::DirectUserStatement,
                    "私聊里说过的内容",
                    None,
                );
                super::world_model::persist_if_dirty().await;
                assert_eq!(world_observation_rows(&pool, person_uuid).await, 1);

                store
                    .delete_person_domain_data(&identity, &direct)
                    .await
                    .expect("擦除应成功");

                // 擦除之后世界里只要再有**任何**活动，脏标记就会被重新置起来，下一次
                // tick 就是"按内存快照整表重写"。被删的人不能借这次重写回来。
                // （少了这一步，第二次 persist 会因为脏标记是 false 而直接返回，
                // 用例就会以错误的理由变绿——这个坑踩过一次。）
                super::world_model::record_observation(
                    WorldScope::Global,
                    ObservationKind::SystemState,
                    ObservationSource::SystemState,
                    "擦除之后别的世界活动",
                    None,
                );
                super::world_model::persist_if_dirty().await;
                assert_eq!(
                    world_observation_rows(&pool, person_uuid).await,
                    0,
                    "擦除之后的下一次持久化不能把被删的人写回来"
                );
            });

        super::world_model::reset_for_tests();
        crate::config::install(previous_config).expect("应还原配置");
    }

    async fn world_observation_rows(pool: &sqlx_postgres::PgPool, person: uuid::Uuid) -> i64 {
        sqlx_core::query_scalar::query_scalar(
            "SELECT COUNT(*) FROM yunxi_world_observations \
             WHERE scope_kind = 'person' AND scope_id = $1",
        )
        .bind(person)
        .fetch_one(pool)
        .await
        .expect("应统计世界模型观察行")
    }

    /// `delete_person_domain_data` 把世界模型那段写成"尽力而为"：
    /// `if world_model_store().is_some() && let Ok(rows) = delete_person_domain_rows(...)`。
    /// 注释说"世界模型失败不能拖垮擦除"，但 PostgreSQL 里任何语句失败都会中止
    /// **整个**事务，后续语句一律 25P02；而 sqlx 的 `commit()` 只是发一条 COMMIT，
    /// 在已中止的事务上等价于回滚，却照样返回 `Ok`。也就是说这个 `let Ok(..)` 恰好
    /// 做不到它想做的事：它会让这次擦除的其余部分一起静默失效。
    ///
    /// 这条测试让世界模型那条 DELETE 真的失败（把它依赖的表删掉），然后断言这个人
    /// 真正该被删掉的记忆仍然被删掉了。
    ///
    /// 它需要 `WORLD_MODEL_STORE` 落在本进程里，因此和 CI 一样按 `--ignored --exact`
    /// 单进程单用例跑；结尾会把 world 表建回来，失败路径也不留残缺 schema。
    #[test]
    #[ignore = "requires PostgreSQL via DATABASE_URL"]
    fn world_store_failure_must_not_take_the_person_erasure_down_with_it() {
        use crate::yunxi::world_model_store::PostgresWorldModelStore;
        use sqlx_postgres::PgPoolOptions;
        use std::sync::Arc;

        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let database_url = std::env::var("DATABASE_URL").expect("需要 DATABASE_URL");
                let pool = PgPoolOptions::new()
                    .max_connections(4)
                    .connect(&database_url)
                    .await
                    .expect("应连接 PostgreSQL");
                let store = initialize_erasure_schemas(&pool).await;

                let world_store = Arc::new(PostgresWorldModelStore::new(pool.clone()));
                world_store
                    .initialize_schema()
                    .await
                    .expect("应初始化 world schema");
                // 让生产代码里那条 `world_model_store().is_some()` 分支成立。
                let _ = super::WORLD_MODEL_STORE.set(Arc::clone(&world_store));

                let (identity, direct, person_uuid) = seed_person_with_memory(&pool, &store).await;

                // 故障注入：世界模型那条 DELETE 必然失败（表不存在）。
                sqlx_core::query::query("DROP TABLE yunxi_world_observations")
                    .execute(&pool)
                    .await
                    .expect("应删表以注入失败");

                let result = store.delete_person_domain_data(&identity, &direct).await;

                let remaining = person_memory_rows(&pool, person_uuid).await;
                // 把表建回来，免得污染的库影响后续用例。
                world_store
                    .initialize_schema()
                    .await
                    .expect("应重建 world schema");
                assert!(
                    result.is_ok(),
                    "身份数据已经删掉了，世界模型失败不该让整次擦除报失败：{result:?}"
                );
                assert_eq!(
                    remaining, 0,
                    "世界模型删失败不该把整次擦除带走（本次调用返回：{result:?}）"
                );
            });
    }
}

/// `[world_model] enabled` 是**总开关**，不是文案开关。这条把它钉住。
///
/// 起因：`bot.conf.example.toml` 那句注释把这个开关描述成"只影响两处文案、不门控任何
/// 行为"——那句话其实描述的是 `shadow_mode`（它确实只往两处状态行拼 `shadow=true`），
/// 但被写在了 `enabled` 这一行下面。照那句话去删门控，就会把总开关拆掉。这里用行为
/// 断言代替注释：关掉之后无论怎么记录都不该有运行时状态，打开之后同一条记录必须进去。
#[cfg(test)]
mod world_model_gating_tests {
    use yunxi_core::world_model::{ObservationKind, ObservationSource, WorldScope};

    fn install_world_model_enabled(enabled: bool) {
        let source = format!("[world_model]\nenabled = {enabled}\n");
        let candidate = crate::config::validate_candidate(&source).expect("候选配置应合法");
        crate::config::install(candidate).expect("应安装测试配置");
    }

    fn record_one_probe_observation() {
        super::world_model::record_observation(
            WorldScope::Global,
            ObservationKind::SystemState,
            ObservationSource::SystemState,
            "总开关测试用的观察",
            None,
        );
    }

    /// 需要改全局配置，所以和其它配置类集成用例一样按 `--ignored --exact` 单独跑
    /// （`ci.yml` 里点名列了它）。它不需要数据库。
    #[test]
    #[ignore = "mutates the process-global config; run via --ignored --exact"]
    fn disabled_world_model_records_nothing() {
        let previous = crate::config::get();

        install_world_model_enabled(false);
        super::world_model::reset_for_tests();
        record_one_probe_observation();
        assert!(
            super::world_model::status_summary().is_none(),
            "enabled=false 时不该存在任何世界模型运行时状态"
        );

        // 状态命令不该拿内存里那份陈旧数据糊弄人。
        assert!(
            super::world_model::world_status_text().contains("未启用"),
            "enabled=false 时 #world-status 应报未启用，而不是渲染旧快照"
        );

        // 反面对照：证明上面不是"记录入口本身坏了"。
        install_world_model_enabled(true);
        super::world_model::reset_for_tests();
        record_one_probe_observation();
        assert!(
            super::world_model::status_summary().is_some(),
            "enabled=true 时同一条观察必须被记下来"
        );
        assert!(
            !super::world_model::world_status_text().contains("未启用"),
            "enabled=true 时状态命令应渲染真实状态"
        );

        // 热关之后持久化不能再写盘：这正是原先漏掉的那一处（store 是启动时建的
        // OnceLock，热关拆不掉它，脏标记也还在）。真正的写盘分支在持久化函数里，
        // 所以把它的前置条件抽成纯函数来钉。
        assert!(
            !super::world_model::should_persist(false, true),
            "关掉之后即使脏也不该写盘"
        );
        assert!(super::world_model::should_persist(true, true));
        assert!(!super::world_model::should_persist(true, false));

        super::world_model::reset_for_tests();
        crate::config::install(previous).expect("应还原配置");
    }
}
