use sqlx_core::query::query;
use sqlx_core::transaction::Transaction;
use sqlx_postgres::Postgres;

/// Serialize additive Yunxi DDL across processes and test threads.
///
/// PostgreSQL's `CREATE TABLE IF NOT EXISTS` is not race-free when two
/// sessions create the same relation for the first time. A transaction-scoped
/// advisory lock keeps every schema initializer behind one database-local
/// migration gate without leaving a session lock behind on error.
pub(super) async fn lock(transaction: &mut Transaction<'_, Postgres>) -> anyhow::Result<()> {
    query("SELECT pg_advisory_xact_lock(hashtext('kovi-bot'), hashtext('yunxi-schema-v1'))")
        .execute(&mut **transaction)
        .await?;
    Ok(())
}
