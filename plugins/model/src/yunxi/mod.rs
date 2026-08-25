mod affect_store;
pub(crate) mod bridge;
pub(crate) mod core_model;
pub(crate) mod delivery;
pub(crate) mod events;
mod goal_store;
mod identity_store;
pub(crate) mod memory_migration;
mod memory_store;
mod open_loop_scheduler;
mod open_loop_store;
mod owner_lock;
pub(crate) mod proactive;
pub(crate) mod qq;
mod relation_store;
mod schema;

use affect_store::PostgresAffectStore;
use anyhow::{Context, Result};
use goal_store::PostgresGoalStore;
use identity_store::PostgresIdentityStore;
use kovi::tokio::sync::{RwLock as AsyncRwLock, RwLockReadGuard};
use memory_store::PostgresMemoryStore;
use open_loop_store::PostgresOpenLoopStore;
use relation_store::PostgresRelationStore;
use std::sync::{Arc, OnceLock, RwLock};
use yunxi_core::{AffectState, IdentityStore, RelationState};

static IDENTITY_STORE: OnceLock<Arc<PostgresIdentityStore>> = OnceLock::new();
static OPEN_LOOP_STORE: OnceLock<Arc<PostgresOpenLoopStore>> = OnceLock::new();
static MEMORY_STORE: OnceLock<Arc<PostgresMemoryStore>> = OnceLock::new();
static AFFECT_STORE: OnceLock<Arc<PostgresAffectStore>> = OnceLock::new();
static RELATION_STORE: OnceLock<Arc<PostgresRelationStore>> = OnceLock::new();
static GOAL_STORE: OnceLock<Arc<PostgresGoalStore>> = OnceLock::new();
static SHADOW_BRIDGE: OnceLock<Arc<bridge::ShadowBridge>> = OnceLock::new();
static DELIVERY_ROUTE_LOCK: AsyncRwLock<()> = AsyncRwLock::const_new(());
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OwnerQqRoute {
    Unconfigured,
    Unavailable,
    Resolved(i64),
}

static OWNER_QQ_ROUTE: OnceLock<RwLock<OwnerQqRoute>> = OnceLock::new();

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
        let store = Arc::new(PostgresGoalStore::new(pool));
        store.initialize_schema().await?;
        let _ = GOAL_STORE.set(store);
    }
    Ok(())
}

async fn initialize_owner_route() {
    cache_owner_route(resolve_owner_route_authoritatively().await);
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
    OWNER_QQ_ROUTE
        .get()
        .map(|cache| cache.read().map_or(OwnerQqRoute::Unavailable, |route| *route))
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

async fn refresh_owner_route() -> OwnerQqRoute {
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

pub(crate) fn install_shadow_bridge(bridge: Arc<bridge::ShadowBridge>) -> Result<()> {
    SHADOW_BRIDGE
        .set(bridge)
        .map_err(|_| anyhow::anyhow!("Yunxi ShadowBridge 已经安装"))
}

pub(crate) async fn begin_qq_user_data_erasure(user_id: i64) -> Result<bridge::UserDataErasure> {
    let bridge = SHADOW_BRIDGE.get().context("Yunxi ShadowBridge 尚未安装")?;
    bridge.begin_user_data_erasure(user_id).await
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
    let _ = refresh_owner_route().await;
    let deleted = deleted?;
    Ok(deleted.total())
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
    let _ = refresh_owner_route().await;
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
    let _ = refresh_owner_route().await;
    let unlinked = unlinked?;
    Ok(unlinked)
}
