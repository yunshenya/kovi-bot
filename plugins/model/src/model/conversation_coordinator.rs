//! 群聊和私聊共享的会话生命周期与排队策略。

use super::interrupt::{
    ReplyScope, ReplyTicket, claim_follow_up_locked, interrupt, interrupt_locked, is_active_locked,
};
use super::recall::begin_reply_locked;
use super::semantic::MessageUnderstanding;
use crate::config;
use crate::vision::VisionImage;
use kovi::Message;
use std::collections::VecDeque;

/// 一个不可拆分的待处理 turn；正文、发送者、附件和消息 ID 总是一起入队。
#[derive(Debug, Clone)]
pub(crate) struct PendingTurn {
    pub(crate) user_id: i64,
    pub(crate) sender: String,
    pub(crate) message: String,
    pub(crate) vision_images: Vec<VisionImage>,
    pub(crate) message_ids: Vec<i32>,
    pub(crate) understanding: MessageUnderstanding,
    pub(crate) sticker_teaching_message: Option<Message>,
}

pub(crate) struct ConversationCoordinator;

impl ConversationCoordinator {
    pub(crate) async fn interrupt(scope: ReplyScope) -> ReplyTicket {
        interrupt(scope).await
    }

    pub(crate) async fn interrupt_locked(scope: ReplyScope) -> ReplyTicket {
        interrupt_locked(scope).await
    }

    pub(crate) async fn is_active_locked(scope: ReplyScope) -> bool {
        is_active_locked(scope).await
    }

    pub(crate) async fn begin_reply_locked(
        scope: ReplyScope,
        ticket: ReplyTicket,
        source_message_ids: Vec<i32>,
    ) -> bool {
        begin_reply_locked(scope, ticket, source_message_ids).await
    }

    pub(crate) async fn claim_follow_up_locked(completed: ReplyTicket) -> Option<ReplyTicket> {
        claim_follow_up_locked(completed).await
    }

    /// 统一队列上限，保证两个入口不会各自演化出不同的丢弃策略。
    pub(crate) fn enqueue(
        queue: &mut VecDeque<PendingTurn>,
        turn: PendingTurn,
        scope_label: &str,
        scope_id: i64,
    ) {
        let max_pending = config::get().traffic().max_pending_turns();
        if queue.len() >= max_pending {
            queue.pop_front();
            eprintln!(
                "[WARN] {}待处理队列已满，丢弃最旧 turn (范围: {}, 上限: {})",
                scope_label, scope_id, max_pending
            );
        }
        queue.push_back(turn);
    }

    /// 领取排队 turn 时必须持有同一会话锁，避免旧 drainer 抢走新消息的代数。
    pub(crate) async fn claim_next_locked(
        scope: ReplyScope,
        completed: &mut ReplyTicket,
        queue: &mut VecDeque<PendingTurn>,
    ) -> Option<(PendingTurn, ReplyTicket)> {
        loop {
            let pending = queue.pop_front()?;
            let Some(ticket) = Self::claim_follow_up_locked(*completed).await else {
                queue.push_front(pending);
                return None;
            };
            if Self::begin_reply_locked(scope, ticket, pending.message_ids.clone()).await {
                return Some((pending, ticket));
            }
            *completed = ticket;
        }
    }
}
