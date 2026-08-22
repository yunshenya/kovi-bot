//! 记忆仓储边界。

use crate::memory::{MEMORY_MANAGER, MemoryEntry, MemoryManager};
use std::sync::Arc;

pub(crate) struct MemoryRepository;

pub(crate) static MEMORY_REPOSITORY: MemoryRepository = MemoryRepository;

impl MemoryRepository {
    pub(crate) fn manager(&self) -> &Arc<MemoryManager> {
        &MEMORY_MANAGER
    }

    pub(crate) async fn add_conversation(
        &self,
        subject_id: i64,
        content: &str,
        context: &str,
        importance: Option<u8>,
        tags: &[String],
    ) -> anyhow::Result<()> {
        self.manager()
            .add_conversation_memory_with_hints(subject_id, content, context, importance, tags)
            .await
    }

    pub(crate) async fn contextual_memories(
        &self,
        subject_id: i64,
        context: &str,
        query: &str,
        limit: usize,
    ) -> Vec<MemoryEntry> {
        self.manager()
            .get_contextual_memories(subject_id, context, query, limit)
            .await
    }

    pub(crate) async fn summary(&self, context: &str, subject_id: i64) -> Option<String> {
        self.manager()
            .get_conversation_summary(context, subject_id)
            .await
    }

    pub(crate) async fn update_summary(
        &self,
        context: &str,
        subject_id: i64,
        summary: String,
    ) -> anyhow::Result<()> {
        self.manager()
            .update_conversation_summary(context, subject_id, summary)
            .await
    }

    pub(crate) async fn personality(&self) -> crate::memory::BotPersonality {
        self.manager().get_bot_personality().await
    }
}
