mod identity_store;
pub(crate) mod qq;

use anyhow::{Context, Result};
use identity_store::PostgresIdentityStore;
use std::sync::{Arc, OnceLock};

static IDENTITY_STORE: OnceLock<Arc<PostgresIdentityStore>> = OnceLock::new();

pub(crate) async fn initialize_database() -> Result<()> {
    if IDENTITY_STORE.get().is_some() {
        return Ok(());
    }

    let pool = crate::memory::MEMORY_MANAGER
        .database_pool()
        .cloned()
        .context("PostgreSQL 连接池尚未初始化")?;
    let store = Arc::new(PostgresIdentityStore::new(pool));
    store.initialize_schema().await?;
    let _ = IDENTITY_STORE.set(store);
    Ok(())
}

#[allow(dead_code)]
pub(crate) fn identity_store() -> Option<Arc<PostgresIdentityStore>> {
    IDENTITY_STORE.get().cloned()
}
