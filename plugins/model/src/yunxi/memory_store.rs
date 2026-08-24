//! Compatibility adapter from the platform-neutral MemoryStore port to the
//! existing bounded MemoryManager. QQ identifiers stay inside this module.

use super::identity_store::PostgresIdentityStore;
use crate::memory::{MemoryEntry, MemoryManager, MemoryType};
use chrono::{DateTime, Local, Utc};
use std::collections::HashSet;
use std::sync::Arc;
use uuid::Uuid;
use yunxi_core::{
    ConversationKind, Memory, MemoryDraft, MemoryId, MemoryKind, MemoryQuery, MemoryScope,
    MemoryStore, MemoryStoreError, MemoryStoreFuture,
};

const PRIVATE_CONTEXT: &str = "private_chat";
const GROUP_CONTEXT: &str = "group_chat";
const GLOBAL_CONTEXT: &str = "yunxi_global:";
const DIRECT_CONTEXT_PREFIX: &str = "yunxi_direct_chat:";
const FACT_CONTEXT_SUFFIX: &str = "|yunxi_kind=fact";
const MAX_LEGACY_SUBJECTS: usize = 32;

#[derive(Clone)]
pub(crate) struct PostgresMemoryStore {
    manager: Arc<MemoryManager>,
    identities: Arc<PostgresIdentityStore>,
}

impl PostgresMemoryStore {
    pub(crate) fn new(manager: Arc<MemoryManager>, identities: Arc<PostgresIdentityStore>) -> Self {
        Self {
            manager,
            identities,
        }
    }

    async fn legacy_scope(&self, scope: MemoryScope) -> Result<LegacyScope, MemoryStoreError> {
        match scope {
            MemoryScope::Global => Ok(LegacyScope {
                subject_ids: Vec::new(),
                context: GLOBAL_CONTEXT.to_owned(),
            }),
            MemoryScope::Person(person_id) => {
                let subject_ids = self
                    .identities
                    .qq_external_identities_for_person(person_id)
                    .await
                    .map_err(MemoryStoreError::storage)?
                    .into_iter()
                    .filter_map(|external| parse_positive_decimal(&external))
                    .take(MAX_LEGACY_SUBJECTS)
                    .collect::<Vec<_>>();
                if subject_ids.is_empty() {
                    return Err(MemoryStoreError::UnsupportedScope { scope });
                }
                Ok(LegacyScope {
                    subject_ids,
                    context: PRIVATE_CONTEXT.to_owned(),
                })
            }
            MemoryScope::Conversation(conversation_id) => {
                let external = self
                    .identities
                    .qq_external_conversations_for_id(conversation_id)
                    .await
                    .map_err(MemoryStoreError::storage)?;
                for (external, stored_kind) in external {
                    if let Some(group_id) = external
                        .strip_prefix("group:")
                        .filter(|value| !value.contains(':'))
                        .and_then(parse_positive_decimal)
                    {
                        if stored_kind == ConversationKind::Group {
                            return Ok(LegacyScope {
                                subject_ids: vec![group_id],
                                context: GROUP_CONTEXT.to_owned(),
                            });
                        }
                        continue;
                    }
                    if parse_direct_external(&external).is_some()
                        && stored_kind == ConversationKind::Direct
                    {
                        return Ok(LegacyScope {
                            // Legacy private memory is person-scoped. A direct
                            // conversation is narrower and must not merge two
                            // bot accounts that happen to share a peer ID.
                            subject_ids: Vec::new(),
                            context: format!("{DIRECT_CONTEXT_PREFIX}{conversation_id}"),
                        });
                    }
                }
                Err(invalid_mapping(
                    "QQ conversation external mapping has an unknown shape or kind",
                ))
            }
        }
    }
}

impl MemoryStore for PostgresMemoryStore {
    fn remember<'a>(&'a self, draft: &'a MemoryDraft) -> MemoryStoreFuture<'a, Memory> {
        Box::pin(async move {
            draft
                .validate()
                .map_err(|error| MemoryStoreError::InvalidRequest {
                    reason: error.to_string(),
                })?;
            let scope = self.legacy_scope(draft.scope()).await?;
            let id = MemoryId::new();
            let context = if draft.kind() == MemoryKind::Fact {
                format!("{}{FACT_CONTEXT_SUFFIX}", scope.context)
            } else {
                scope.context.clone()
            };
            let entry = MemoryEntry {
                id: id.to_string(),
                content: draft.content().to_string(),
                timestamp: draft.occurred_at().with_timezone(&Local),
                memory_type: legacy_kind(draft.kind()),
                importance: importance_to_legacy(draft.importance()),
                tags: draft.tags().to_vec(),
                context,
                subject_id: scope.subject_ids.first().copied(),
            };
            self.manager
                .add_memory(entry)
                .await
                .map_err(storage_error)?;
            Memory::from_draft(id, draft, Utc::now()).map_err(|error| {
                MemoryStoreError::InvalidRequest {
                    reason: error.to_string(),
                }
            })
        })
    }

    fn recall<'a>(&'a self, query: &'a MemoryQuery) -> MemoryStoreFuture<'a, Vec<Memory>> {
        Box::pin(async move {
            let scope = self.legacy_scope(query.scope()).await?;
            let fetch_limit = query.limit().saturating_mul(4).min(128);
            let mut entries = Vec::new();
            let subject_ids = if scope.subject_ids.is_empty() {
                vec![None]
            } else {
                scope.subject_ids.iter().copied().map(Some).collect()
            };
            for subject_id in subject_ids {
                entries.extend(
                    self.manager
                        .get_recent_memories_for_domain_scope(
                            subject_id,
                            &scope.context,
                            fetch_limit,
                        )
                        .await,
                );
            }
            let mut seen_ids = HashSet::new();
            entries.retain(|entry| seen_ids.insert(entry.id.clone()));
            let mut matches = entries
                .into_iter()
                .filter(|entry| {
                    query.min_importance().is_none_or(|minimum| {
                        u16::from(entry.importance) * 10 >= u16::from(minimum)
                    })
                })
                .filter(|entry| query.text().trim().is_empty() || matches_text(entry, query.text()))
                .collect::<Vec<_>>();
            matches.sort_by(|left, right| {
                relevance(right, query.text())
                    .cmp(&relevance(left, query.text()))
                    .then_with(|| right.timestamp.cmp(&left.timestamp))
            });
            matches.truncate(query.limit());
            Ok(matches
                .into_iter()
                .filter_map(|entry| to_core_memory(query.scope(), entry).ok())
                .collect())
        })
    }

    fn forget(&self, scope: MemoryScope, id: MemoryId) -> MemoryStoreFuture<'_, bool> {
        Box::pin(async move {
            let scope = self.legacy_scope(scope).await?;
            let subject_ids = if scope.subject_ids.is_empty() {
                vec![None]
            } else {
                scope.subject_ids.iter().copied().map(Some).collect()
            };
            for subject_id in subject_ids {
                if self
                    .manager
                    .delete_memory_for_domain_scope(&id.to_string(), subject_id, &scope.context)
                    .await
                    .map_err(storage_error)?
                {
                    return Ok(true);
                }
            }
            Ok(false)
        })
    }
}

#[derive(Debug, Clone)]
struct LegacyScope {
    subject_ids: Vec<i64>,
    context: String,
}

fn parse_direct_external(value: &str) -> Option<(i64, i64)> {
    let mut parts = value.strip_prefix("direct:")?.split(':');
    let self_id = parse_positive_decimal(parts.next()?)?;
    let peer_id = parse_positive_decimal(parts.next()?)?;
    parts.next().is_none().then_some((self_id, peer_id))
}

fn parse_positive_decimal(value: &str) -> Option<i64> {
    let value = value.parse::<i64>().ok()?;
    (value > 0).then_some(value)
}

fn invalid_mapping(reason: &'static str) -> MemoryStoreError {
    MemoryStoreError::InvalidRequest {
        reason: reason.to_string(),
    }
}

fn storage_error(error: anyhow::Error) -> MemoryStoreError {
    MemoryStoreError::storage(std::io::Error::other(error.to_string()))
}

fn legacy_kind(kind: MemoryKind) -> MemoryType {
    match kind {
        MemoryKind::Conversation => MemoryType::Conversation,
        MemoryKind::Profile => MemoryType::UserProfile,
        MemoryKind::Event | MemoryKind::Fact => MemoryType::Event,
        MemoryKind::Preference => MemoryType::Preference,
        MemoryKind::Emotion => MemoryType::Emotion,
    }
}

fn importance_to_legacy(importance: u8) -> u8 {
    // Legacy storage has ten buckets. Preserve any non-zero Core importance
    // instead of truncating 1..9 to an apparently forgotten value.
    importance.saturating_add(9).saturating_div(10).min(10)
}

fn matches_text(entry: &MemoryEntry, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return true;
    }
    let searchable = format!(
        "{} {}",
        entry.content.to_lowercase(),
        entry.tags.join(" ").to_lowercase()
    );
    searchable.contains(&query)
        || query
            .split_whitespace()
            .any(|term| searchable.contains(term))
}

fn relevance(entry: &MemoryEntry, query: &str) -> u8 {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return entry.importance;
    }
    let searchable = format!(
        "{} {}",
        entry.content.to_lowercase(),
        entry.tags.join(" ").to_lowercase()
    );
    let exact = u8::from(searchable.contains(&query)).saturating_mul(10);
    exact.saturating_add(entry.importance)
}

fn to_core_memory(scope: MemoryScope, entry: MemoryEntry) -> Result<Memory, MemoryStoreError> {
    let id = Uuid::parse_str(&entry.id)
        .map(MemoryId::from_uuid)
        .unwrap_or_else(|_| {
            MemoryId::from_uuid(Uuid::new_v5(&Uuid::NAMESPACE_URL, entry.id.as_bytes()))
        });
    let occurred_at: DateTime<Utc> = entry.timestamp.with_timezone(&Utc);
    let draft = MemoryDraft::new(
        scope,
        core_kind(entry.memory_type, &entry.context),
        entry.content,
        occurred_at,
    )
    .map_err(|error| MemoryStoreError::InvalidRequest {
        reason: error.to_string(),
    })?
    .with_importance(entry.importance.saturating_mul(10))
    .and_then(|draft| draft.with_tags(entry.tags))
    .map_err(|error| MemoryStoreError::InvalidRequest {
        reason: error.to_string(),
    })?;
    Memory::from_draft(id, &draft, occurred_at).map_err(|error| MemoryStoreError::InvalidRequest {
        reason: error.to_string(),
    })
}

fn core_kind(kind: MemoryType, context: &str) -> MemoryKind {
    match kind {
        MemoryType::Conversation => MemoryKind::Conversation,
        MemoryType::UserProfile | MemoryType::GroupInfo => MemoryKind::Profile,
        MemoryType::Event if context.ends_with(FACT_CONTEXT_SUFFIX) => MemoryKind::Fact,
        MemoryType::Event => MemoryKind::Event,
        MemoryType::Preference => MemoryKind::Preference,
        MemoryType::Emotion => MemoryKind::Emotion,
    }
}

#[cfg(test)]
mod tests {
    use super::{core_kind, importance_to_legacy, parse_direct_external, parse_positive_decimal};
    use crate::memory::MemoryType;
    use yunxi_core::MemoryKind;

    #[test]
    fn external_numeric_mapping_never_accepts_non_positive_values() {
        assert_eq!(parse_positive_decimal("123"), Some(123));
        assert_eq!(parse_positive_decimal("0"), None);
        assert_eq!(parse_positive_decimal("-1"), None);
        assert_eq!(parse_positive_decimal("nickname"), None);
    }

    #[test]
    fn direct_external_keys_are_strictly_bounded() {
        assert_eq!(parse_direct_external("direct:10:20"), Some((10, 20)));
        assert_eq!(parse_direct_external("direct:10:20:30"), None);
        assert_eq!(parse_direct_external("direct:bot:20"), None);
        assert_eq!(parse_direct_external("direct:0:20"), None);
    }

    #[test]
    fn fact_kind_and_importance_keep_a_bounded_legacy_projection() {
        assert_eq!(
            core_kind(MemoryType::Event, "private_chat|yunxi_kind=fact"),
            MemoryKind::Fact
        );
        assert_eq!(
            core_kind(MemoryType::Event, "private_chat"),
            MemoryKind::Event
        );
        assert_eq!(importance_to_legacy(0), 0);
        assert_eq!(importance_to_legacy(1), 1);
        assert_eq!(importance_to_legacy(100), 10);
    }
}
