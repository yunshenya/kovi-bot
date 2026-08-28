//! 群聊连续会话的有界语义状态。
//!
//! 会话是否继续由语义关系、显式结束和换题决定。这里不使用时间窗口
//! 作为回复资格；时间只属于上层的限流和运行时状态清理。

use super::semantic::MessageUnderstanding;
use std::collections::{HashMap, VecDeque};

const MAX_PARTICIPANTS: usize = 64;
const MAX_PENDING_TURNS: usize = 64;
const MAX_TOPICS: usize = 8;
const MAX_TOPIC_CHARS: usize = 80;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ConversationTurnOptions {
    pub(crate) reset_context: bool,
    pub(crate) close_after_reply: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ConversationDecision {
    pub(crate) continue_reply: bool,
    pub(crate) turn: ConversationTurnOptions,
}

#[derive(Debug, Clone)]
struct PendingConversationTurn {
    generation: u64,
    options: ConversationTurnOptions,
    topics: Vec<String>,
}

/// State for one group. Natural-language fields stay bounded and are only
/// hints for semantic comparison; they never become executable instructions.
#[derive(Debug, Default)]
pub(crate) struct GroupConversationState {
    active: bool,
    participants: HashMap<i64, u64>,
    topics: VecDeque<String>,
    pending: HashMap<i64, PendingConversationTurn>,
    sequence: u64,
}

impl GroupConversationState {
    pub(crate) fn is_active(&self) -> bool {
        self.active || !self.pending.is_empty()
    }

    pub(crate) fn context(&self) -> String {
        if self.topics.is_empty() {
            "（当前没有已确认的群聊主题）".to_owned()
        } else {
            self.topics.iter().cloned().collect::<Vec<_>>().join("、")
        }
    }

    /// Classify the incoming turn using semantic relation rather than age.
    /// Unrelated ambient traffic is observed but does not close another
    /// participant's active conversation.
    pub(crate) fn observe(
        &mut self,
        _user_id: i64,
        understanding: &MessageUnderstanding,
        direct_reply_expected: bool,
    ) -> ConversationDecision {
        let active = self.is_active();
        let same_topic = understanding.conversation_relevant && !understanding.topic_shift;
        let explicit_end = understanding.conversation_end
            || understanding.wants_stop
            || understanding.wants_no_reply;

        if direct_reply_expected {
            return ConversationDecision {
                continue_reply: false,
                turn: ConversationTurnOptions {
                    reset_context: !active || !same_topic,
                    close_after_reply: understanding.conversation_end,
                },
            };
        }

        if !active {
            return ConversationDecision::default();
        }
        if explicit_end {
            self.close();
            return ConversationDecision::default();
        }
        if !same_topic {
            return ConversationDecision::default();
        }

        ConversationDecision {
            continue_reply: true,
            turn: ConversationTurnOptions::default(),
        }
    }

    pub(crate) fn begin_turn(
        &mut self,
        user_id: i64,
        options: ConversationTurnOptions,
        topics: &[String],
    ) -> u64 {
        self.sequence = self.sequence.wrapping_add(1).max(1);
        let generation = self.sequence;
        self.pending.insert(
            user_id,
            PendingConversationTurn {
                generation,
                options,
                topics: topics.to_vec(),
            },
        );
        generation
    }

    pub(crate) fn finish_turn(&mut self, user_id: i64, generation: u64, replied: bool) {
        let Some(pending) = self.pending.get(&user_id) else {
            return;
        };
        if pending.generation != generation {
            return;
        }
        let pending = self
            .pending
            .remove(&user_id)
            .expect("pending conversation turn must still exist");
        if !replied {
            return;
        }

        if pending.options.reset_context {
            self.active = true;
            self.participants.clear();
            self.topics.clear();
        }
        if pending.options.close_after_reply {
            self.active = false;
            self.participants.clear();
            self.topics.clear();
            return;
        }
        self.active = true;
        self.touch_participant(user_id);
        self.add_topics(&pending.topics);
    }

    pub(crate) fn close(&mut self) {
        self.active = false;
        self.participants.clear();
        self.topics.clear();
        self.pending.clear();
    }

    pub(crate) fn prune(&mut self) {
        while self.participants.len() > MAX_PARTICIPANTS {
            let Some((oldest, _)) = self
                .participants
                .iter()
                .min_by_key(|(_, sequence)| *sequence)
                .map(|(user_id, sequence)| (*user_id, *sequence))
            else {
                break;
            };
            self.participants.remove(&oldest);
        }
        self.pending.retain(|_, pending| {
            pending.generation == self.sequence
                || self.sequence.wrapping_sub(pending.generation) < MAX_PENDING_TURNS as u64
        });
        while self.pending.len() > MAX_PENDING_TURNS {
            let Some((oldest, _)) = self
                .pending
                .iter()
                .min_by_key(|(_, pending)| pending.generation)
                .map(|(user_id, pending)| (*user_id, pending.generation))
            else {
                break;
            };
            self.pending.remove(&oldest);
        }
    }

    #[cfg(test)]
    fn participant_count(&self) -> usize {
        self.participants.len()
    }

    #[cfg(test)]
    fn topic_count(&self) -> usize {
        self.topics.len()
    }

    fn touch_participant(&mut self, user_id: i64) {
        self.sequence = self.sequence.wrapping_add(1).max(1);
        self.participants.insert(user_id, self.sequence);
    }

    fn add_topics(&mut self, topics: &[String]) {
        for topic in topics {
            let topic = normalize_topic(topic);
            if topic.is_empty() {
                continue;
            }
            self.topics.retain(|existing| existing != &topic);
            self.topics.push_back(topic);
            while self.topics.len() > MAX_TOPICS {
                self.topics.pop_front();
            }
        }
    }
}

fn normalize_topic(topic: &str) -> String {
    topic
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(MAX_TOPIC_CHARS)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{ConversationTurnOptions, GroupConversationState};
    use crate::model::semantic::MessageUnderstanding;

    fn understanding(relevant: bool, topic_shift: bool) -> MessageUnderstanding {
        MessageUnderstanding {
            conversation_relevant: relevant,
            topic_shift,
            topics: vec!["当前问题".to_owned()],
            ..MessageUnderstanding::default()
        }
    }

    #[test]
    fn relevant_turn_continues_without_a_time_window() {
        let mut state = GroupConversationState::default();
        let marker = state.begin_turn(
            42,
            ConversationTurnOptions {
                reset_context: true,
                close_after_reply: false,
            },
            &["面试".to_owned()],
        );
        state.finish_turn(42, marker, true);

        let decision = state.observe(99, &understanding(true, false), false);
        assert!(decision.continue_reply);
        assert!(state.is_active());
        assert!(state.context().contains("面试"));
        let next = state.begin_turn(99, decision.turn, &["当前问题".to_owned()]);
        state.finish_turn(99, next, true);
        assert!(state.context().contains("当前问题"));
    }

    #[test]
    fn unrelated_ambient_traffic_does_not_close_the_active_topic() {
        let mut state = GroupConversationState::default();
        let marker = state.begin_turn(
            42,
            ConversationTurnOptions {
                reset_context: true,
                close_after_reply: false,
            },
            &["面试".to_owned()],
        );
        state.finish_turn(42, marker, true);

        let decision = state.observe(99, &understanding(false, true), false);
        assert!(!decision.continue_reply);
        assert!(state.is_active());
        assert!(state.context().contains("面试"));
    }

    #[test]
    fn direct_topic_shift_replaces_context_only_after_successful_reply() {
        let mut state = GroupConversationState::default();
        let marker = state.begin_turn(
            42,
            ConversationTurnOptions {
                reset_context: true,
                close_after_reply: false,
            },
            &["旧话题".to_owned()],
        );
        state.finish_turn(42, marker, true);

        let decision = state.observe(42, &understanding(false, true), true);
        assert!(decision.turn.reset_context);
        assert!(state.context().contains("旧话题"));
        let next = state.begin_turn(42, decision.turn, &["新话题".to_owned()]);
        state.finish_turn(42, next, true);
        assert!(!state.context().contains("旧话题"));
        assert!(state.context().contains("新话题"));
    }

    #[test]
    fn explicit_end_closes_after_the_final_reply() {
        let mut state = GroupConversationState::default();
        let marker = state.begin_turn(
            42,
            ConversationTurnOptions {
                reset_context: true,
                close_after_reply: false,
            },
            &["聊天".to_owned()],
        );
        state.finish_turn(42, marker, true);
        let mut end = understanding(true, false);
        end.conversation_end = true;
        let decision = state.observe(42, &end, true);
        assert!(decision.turn.close_after_reply);
        let next = state.begin_turn(42, decision.turn, &[]);
        state.finish_turn(42, next, true);
        assert!(!state.is_active());
        assert_eq!(state.context(), "（当前没有已确认的群聊主题）");
    }

    #[test]
    fn participant_and_topic_memory_stay_bounded() {
        let mut state = GroupConversationState::default();
        for user_id in 0..80 {
            let marker = state.begin_turn(
                user_id,
                ConversationTurnOptions {
                    reset_context: user_id == 0,
                    close_after_reply: false,
                },
                &[format!("主题 {user_id}")],
            );
            state.finish_turn(user_id, marker, true);
        }
        state.prune();
        assert!(state.participant_count() <= 64);
        assert!(state.topic_count() <= 8);
    }
}
