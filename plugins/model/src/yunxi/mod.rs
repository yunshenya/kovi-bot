pub(crate) mod bridge;
mod identity_store;
mod memory_store;
mod open_loop_scheduler;
mod open_loop_store;
pub(crate) mod qq;

use anyhow::{Context, Result};
use identity_store::PostgresIdentityStore;
use memory_store::PostgresMemoryStore;
use open_loop_store::PostgresOpenLoopStore;
use std::sync::{Arc, OnceLock};

static IDENTITY_STORE: OnceLock<Arc<PostgresIdentityStore>> = OnceLock::new();
static OPEN_LOOP_STORE: OnceLock<Arc<PostgresOpenLoopStore>> = OnceLock::new();
static MEMORY_STORE: OnceLock<Arc<PostgresMemoryStore>> = OnceLock::new();

pub(crate) async fn initialize_database() -> Result<()> {
    if IDENTITY_STORE.get().is_some()
        && OPEN_LOOP_STORE.get().is_some()
        && MEMORY_STORE.get().is_some()
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
        let store = Arc::new(PostgresOpenLoopStore::new(pool));
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
        ));
        let _ = MEMORY_STORE.set(store);
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
