mod affect_store;
pub(crate) mod bridge;
pub(crate) mod core_model;
pub(crate) mod delivery;
mod goal_store;
mod identity_store;
mod memory_store;
mod open_loop_scheduler;
mod open_loop_store;
mod owner_lock;
pub(crate) mod proactive;
pub(crate) mod qq;
mod relation_store;

use affect_store::PostgresAffectStore;
use anyhow::{Context, Result};
use goal_store::PostgresGoalStore;
use identity_store::PostgresIdentityStore;
use memory_store::PostgresMemoryStore;
use open_loop_store::PostgresOpenLoopStore;
use relation_store::PostgresRelationStore;
use std::sync::{Arc, OnceLock};

static IDENTITY_STORE: OnceLock<Arc<PostgresIdentityStore>> = OnceLock::new();
static OPEN_LOOP_STORE: OnceLock<Arc<PostgresOpenLoopStore>> = OnceLock::new();
static MEMORY_STORE: OnceLock<Arc<PostgresMemoryStore>> = OnceLock::new();
static AFFECT_STORE: OnceLock<Arc<PostgresAffectStore>> = OnceLock::new();
static RELATION_STORE: OnceLock<Arc<PostgresRelationStore>> = OnceLock::new();
static GOAL_STORE: OnceLock<Arc<PostgresGoalStore>> = OnceLock::new();
static SHADOW_BRIDGE: OnceLock<Arc<bridge::ShadowBridge>> = OnceLock::new();

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
    let store = IDENTITY_STORE
        .get()
        .context("Yunxi identity store 尚未初始化")?;
    let external_identity = qq::person(user_id)?;
    let direct_conversation = qq::direct(self_id, user_id)?;
    let deleted = store
        .delete_person_domain_data(&external_identity, &direct_conversation)
        .await
        .map_err(anyhow::Error::from)?;
    Ok(deleted.total())
}
