use sqlx_core::query::query;
use sqlx_core::query_scalar::query_scalar;
use sqlx_core::transaction::Transaction;
use sqlx_postgres::Postgres;
use uuid::Uuid;

/// Canonical durable owners shared by Yunxi stores.
///
/// The tables use a polymorphic `(owner_kind, owner_id)` pair, so PostgreSQL
/// cannot express the Person/Conversation relationship with one ordinary
/// foreign key. Every writer and the person-domain deletion path therefore
/// serialize on this same transaction-scoped advisory lock and writers verify
/// that the canonical owner still exists before inserting a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DurableOwner {
    Person(Uuid),
    Conversation(Uuid),
    Global,
}

impl DurableOwner {
    fn lock_key(self) -> String {
        match self {
            Self::Person(id) => format!("yunxi-owner:person:{id}"),
            Self::Conversation(id) => format!("yunxi-owner:conversation:{id}"),
            Self::Global => "yunxi-owner:global".to_string(),
        }
    }
}

pub(crate) async fn lock_owner(
    transaction: &mut Transaction<'_, Postgres>,
    owner: DurableOwner,
) -> Result<(), sqlx_core::error::Error> {
    query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(owner.lock_key())
        .execute(&mut **transaction)
        .await?;
    Ok(())
}

/// Serialize operations that touch both the canonical memory table and its
/// legacy compatibility projection. This lock is deliberately independent of
/// an owner lock because retention and migration may span many owners.
pub(crate) async fn lock_memory_maintenance(
    transaction: &mut Transaction<'_, Postgres>,
) -> Result<(), sqlx_core::error::Error> {
    query("SELECT pg_advisory_xact_lock(hashtext('kovi-bot'), hashtext('yunxi-memory-maintenance-v1'))")
        .execute(&mut **transaction)
        .await?;
    Ok(())
}

/// Take a shared snapshot barrier for memory reads. Multiple recalls may run
/// concurrently, while an exclusive maintenance lock still prevents a read
/// from mixing Core rows with a half-completed compatibility/identity update.
pub(crate) async fn lock_memory_read(
    transaction: &mut Transaction<'_, Postgres>,
) -> Result<(), sqlx_core::error::Error> {
    query("SELECT pg_advisory_xact_lock_shared(hashtext('kovi-bot'), hashtext('yunxi-memory-maintenance-v1'))")
        .execute(&mut **transaction)
        .await?;
    Ok(())
}

pub(crate) async fn lock_and_owner_exists(
    transaction: &mut Transaction<'_, Postgres>,
    owner: DurableOwner,
) -> Result<bool, sqlx_core::error::Error> {
    lock_owner(transaction, owner).await?;
    match owner {
        DurableOwner::Person(id) => {
            query_scalar::<Postgres, bool>(
                "SELECT EXISTS (SELECT 1 FROM yunxi_persons WHERE id = $1)",
            )
            .bind(id)
            .fetch_one(&mut **transaction)
            .await
        }
        DurableOwner::Conversation(id) => {
            query_scalar::<Postgres, bool>(
                "SELECT EXISTS (SELECT 1 FROM yunxi_conversations WHERE id = $1)",
            )
            .bind(id)
            .fetch_one(&mut **transaction)
            .await
        }
        DurableOwner::Global => Ok(true),
    }
}
