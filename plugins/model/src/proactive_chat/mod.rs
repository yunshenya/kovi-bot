//! # 主动聊天模块
//!
//! 提供智能主动聊天功能，包括：
//! - 基于情绪和社交信心的主动聊天判断
//! - 智能目标选择（群聊或私聊）
//! - 活跃度检测和时机判断
//! - 话题生成和个性化聊天

use crate::group_access;
use crate::memory::MemoryManager;
use crate::model::semantic::{MessageUnderstanding, UnderstandingRequest, understand};
use crate::model::{
    MessageDestination, OutgoingSource, TrackedSendError, send_tracked_message_with_revalidation,
};
use crate::mood_system::MOOD_SYSTEM;
use crate::topic_generator::TopicGenerator;
use crate::yunxi;
use crate::yunxi::bridge::ShadowBridge;
use anyhow::Result;
use chrono::Local;
use kovi::tokio::time::sleep;
use kovi::{Message, RuntimeBot};
use rand::Rng;
use rand::prelude::IndexedRandom;
use std::sync::Arc;
use std::time::Duration;
use yunxi_core::{
    ActionPortOutcome, ActionResult, MessageContent, ProactiveOpportunity, ProposedAction,
    ReachOutAction, ReachOutIntent,
};

pub mod startup;

const GLOBAL_PROACTIVE_STATE_KEY: &str = "proactive:global";

fn prepared_grace_duration(configured_ms: u64) -> Duration {
    if configured_ms == 0 {
        Duration::ZERO
    } else {
        Duration::from_millis(configured_ms.clamp(300, 1_000))
    }
}

fn target_state_key(scope: &str, subject_id: i64) -> String {
    format!("proactive:{scope}:{subject_id}")
}

fn main_admin_state_key(subject_id: i64) -> String {
    target_state_key("main_admin", subject_id)
}

async fn configured_owner_target() -> Option<i64> {
    match yunxi::canonical_owner_qq_id_authoritative().await {
        Some(route) => route,
        None => crate::config::get().proactive().main_admin(),
    }
}

/// 主动聊天管理器
///
/// 负责管理机器人的主动聊天行为，包括判断时机、选择目标、生成话题等
pub struct ProactiveChatManager {
    /// 记忆管理器，用于获取用户和群组信息
    memory_manager: Arc<MemoryManager>,
    /// 话题生成器，用于生成个性化话题
    topic_generator: TopicGenerator,
    /// 机器人实例，用于发送消息
    bot: Arc<RuntimeBot>,
    /// Shared Core bridge used only for best-effort idle observations.
    yunxi_bridge: Option<Arc<ShadowBridge>>,
}

impl ProactiveChatManager {
    pub fn new(memory_manager: Arc<MemoryManager>, bot: Arc<RuntimeBot>) -> Self {
        Self::new_with_bridge(memory_manager, bot, None)
    }

    pub(crate) fn new_with_bridge(
        memory_manager: Arc<MemoryManager>,
        bot: Arc<RuntimeBot>,
        yunxi_bridge: Option<Arc<ShadowBridge>>,
    ) -> Self {
        let topic_generator = TopicGenerator::new(Arc::clone(&memory_manager));
        Self {
            memory_manager,
            topic_generator,
            bot,
            yunxi_bridge,
        }
    }

    pub async fn start_proactive_chat_loop(&self) {
        // 服务重启后先等待一个检查周期，避免启动瞬间把重启误判成新的主动时机。
        let initial_interval = crate::config::get().proactive().check_interval_secs();
        sleep(Duration::from_secs(initial_interval)).await;
        loop {
            let proactive_config = crate::config::get().proactive().clone();
            if !proactive_config.enabled() {
                sleep(Duration::from_secs(proactive_config.check_interval_secs())).await;
                continue;
            }
            if let Some(bridge) = &self.yunxi_bridge {
                bridge.observe_idle_tick();
            }

            // 最信任用户有独立、严格限额的关心频率，不与群聊随机目标竞争。
            let main_admin_message_sent = match self.try_initiate_main_admin_chat().await {
                Ok(sent) => sent,
                Err(error) => {
                    eprintln!("Failed to initiate main-admin chat: {}", error);
                    false
                }
            };

            // 常规主动聊天仍遵循全局冷却与活跃度限制。
            if !main_admin_message_sent
                && self.should_initiate_chat().await
                && let Err(e) = self.try_initiate_chat().await
            {
                eprintln!("Failed to initiate chat: {}", e);
            }

            // 等待一段时间再检查
            sleep(Duration::from_secs(proactive_config.check_interval_secs())).await;
        }
    }

    async fn should_initiate_chat(&self) -> bool {
        if !self.can_send_regular_chat().await {
            return false;
        }
        let probability = crate::config::get().proactive().push_probability_percent() as u32;
        probability > 0 && rand::rng().random_ratio(probability, 100)
    }

    async fn can_send_regular_chat(&self) -> bool {
        let personality = self.memory_manager.get_bot_personality().await;

        // 检查基本条件
        if personality.energy_level < 5 || personality.social_confidence < 4 {
            return false;
        }

        let proactive_config = crate::config::get().proactive().clone();
        let now = Local::now();
        let inactivity_boundary =
            now - chrono::Duration::seconds(proactive_config.inactivity_threshold_secs() as i64);
        let cooldown_boundary =
            now - chrono::Duration::seconds(proactive_config.cooldown_secs() as i64);
        let today = now.format("%Y-%m-%d").to_string();
        let global_state = self
            .memory_manager
            .get_proactive_state(GLOBAL_PROACTIVE_STATE_KEY)
            .await;

        // 全局冷却和每日上限使用独立持久化状态；旧记忆只作为升级期间的兼容兜底。
        let durable_global_cooldown = global_state
            .as_ref()
            .and_then(|state| state.last_sent_at)
            .is_some_and(|last_sent| last_sent > cooldown_boundary);
        let legacy_global_cooldown = self
            .memory_manager
            .has_memory_since_in_contexts(
                &["proactive_group_chat", "proactive_private_chat"],
                cooldown_boundary,
            )
            .await;
        if durable_global_cooldown || legacy_global_cooldown {
            return false;
        }
        if global_state.as_ref().is_some_and(|state| {
            state.daily_count_for(&today) >= proactive_config.daily_limit() as u32
        }) {
            return false;
        }

        let recent_activity_count = self
            .memory_manager
            .count_non_proactive_memories_since(inactivity_boundary, 3)
            .await;

        if recent_activity_count >= 3 {
            return false;
        }

        true
    }

    async fn try_initiate_chat(&self) -> Result<()> {
        // 获取所有群组和用户
        let groups = self.get_active_groups().await;
        let users = self.get_active_users().await;

        // 随机选择一个目标
        let target = self.select_chat_target(groups, users).await;

        match target {
            ChatTarget::Group(group_id) => {
                self.initiate_group_chat(group_id).await?;
            }
            ChatTarget::User(user_id) => {
                self.initiate_private_chat(user_id).await?;
            }
            ChatTarget::None => {
                // 没有合适的目标，跳过这次主动聊天
            }
        }

        Ok(())
    }

    async fn plan_private_reach_out(&self, user_id: i64) -> Result<Option<ProactiveOpportunity>> {
        let Some(identity_store) = yunxi::identity_store() else {
            kovi::log::warn!("Yunxi proactive identity store is unavailable");
            return Ok(None);
        };
        let external = match yunxi::qq::person(user_id) {
            Ok(external) => external,
            Err(error) => {
                kovi::log::warn!("Yunxi proactive identity reference is invalid: {error}");
                return Ok(None);
            }
        };
        let person_id = match identity_store.resolve_identity(&external).await {
            Ok(person_id) => person_id,
            Err(error) => {
                kovi::log::warn!("Yunxi proactive identity resolution failed: {error}");
                return Ok(None);
            }
        };
        let Some(profile) = self.memory_manager.get_user_profile(user_id).await else {
            return Ok(None);
        };
        let personality = self.memory_manager.get_bot_personality().await;
        let memories = self
            .memory_manager
            .get_recent_memories_for_subject(user_id, Some("private"), 16)
            .await;
        yunxi::proactive::project_private_opportunity(
            person_id,
            &profile,
            &personality,
            &memories,
            Local::now(),
            crate::model::utils::model_load_percent(),
        )
        .map_err(Into::into)
    }

    async fn generate_private_reach_out(&self, user_id: i64) -> Result<Option<ReachOutIntent>> {
        let Some(opportunity) = self.plan_private_reach_out(user_id).await? else {
            return Ok(None);
        };
        let motive = opportunity.motive();
        let Some(topic) = self
            .topic_generator
            .generate_memory_based_topic_with_motive(None, Some(user_id), Some(motive))
            .await?
        else {
            return Ok(None);
        };
        if topic.proactive_motive != Some(motive) {
            return Ok(None);
        }
        ReachOutIntent::from_opportunity(opportunity, MessageContent::text(topic.content))
            .map(Some)
            .map_err(Into::into)
    }

    async fn deliver_private_reach_out(&self, user_id: i64, intent: &ReachOutIntent) -> bool {
        if let Some(bridge) = &self.yunxi_bridge {
            let Ok(action) = ReachOutAction::from_intent(intent.clone()) else {
                return false;
            };
            return matches!(
                bridge
                    .dispatch_action(user_id, ProposedAction::ReachOut(action))
                    .await,
                Some(ActionResult::Executed {
                    outcome: ActionPortOutcome::Delivered { .. },
                    ..
                })
            );
        }
        let Some(identity_store) = yunxi::identity_store() else {
            return false;
        };
        yunxi::delivery::send_reach_out(&self.bot, &identity_store, intent, user_id).await
    }

    /// 由模型结合关系、情绪与近期互动决定是否主动关心最信任用户。
    async fn try_initiate_main_admin_chat(&self) -> Result<bool> {
        let Some(main_admin) = configured_owner_target().await else {
            return Ok(false);
        };
        if !self.can_send_main_admin(main_admin).await {
            return Ok(false);
        }
        if !self.should_decide_main_admin_chat(main_admin).await {
            return Ok(false);
        }

        let decision_key = main_admin_state_key(main_admin);
        self.memory_manager
            .record_proactive_event(Some(&decision_key), &[], Local::now())
            .await?;

        let Some(intent) = self.generate_private_reach_out(main_admin).await? else {
            return Ok(false);
        };
        let message = intent.message().as_text().to_string();

        // Model generation can take long enough for another event to invalidate
        // cooldown, daily-limit, or recent-interaction state.
        if !self.can_send_main_admin(main_admin).await
            || yunxi::canonical_owner_matches_authoritative(main_admin).await != Some(true)
            || !self.deliver_private_reach_out(main_admin, &intent).await
        {
            return Ok(false);
        }
        let target_key = target_state_key("private", main_admin);
        self.memory_manager
            .record_proactive_event(
                None,
                &[
                    GLOBAL_PROACTIVE_STATE_KEY.to_string(),
                    target_key,
                    decision_key,
                ],
                Local::now(),
            )
            .await?;
        self.memory_manager
            .add_conversation_memory(
                main_admin,
                &format!("主动关心: {}", message),
                "proactive_private_chat",
            )
            .await?;
        Ok(true)
    }

    async fn can_send_main_admin(&self, main_admin: i64) -> bool {
        let proactive_config = crate::config::get().proactive().clone();
        let now = Local::now();
        let today = now.format("%Y-%m-%d").to_string();
        let boundary =
            now - chrono::Duration::seconds(proactive_config.main_admin_cooldown_secs() as i64);
        let target_boundary =
            now - chrono::Duration::seconds(proactive_config.target_cooldown_secs() as i64);
        let global_boundary =
            now - chrono::Duration::seconds(proactive_config.cooldown_secs() as i64);
        let main_key = main_admin_state_key(main_admin);
        let target_key = target_state_key("private", main_admin);
        let main_state = self.memory_manager.get_proactive_state(&main_key).await;
        let target_state = self.memory_manager.get_proactive_state(&target_key).await;
        let global_state = self
            .memory_manager
            .get_proactive_state(GLOBAL_PROACTIVE_STATE_KEY)
            .await;

        if main_state
            .as_ref()
            .and_then(|state| state.last_sent_at)
            .is_some_and(|last_sent| last_sent > boundary)
            || target_state
                .as_ref()
                .and_then(|state| state.last_sent_at)
                .is_some_and(|last_sent| last_sent > target_boundary)
            || global_state
                .as_ref()
                .and_then(|state| state.last_sent_at)
                .is_some_and(|last_sent| last_sent > global_boundary)
        {
            return false;
        }
        if main_state.as_ref().is_some_and(|state| {
            state.daily_count_for(&today) >= proactive_config.main_admin_daily_limit() as u32
        }) || global_state.as_ref().is_some_and(|state| {
            state.daily_count_for(&today) >= proactive_config.daily_limit() as u32
        }) {
            return false;
        }

        if let Some(profile) = self.memory_manager.get_user_profile(main_admin).await {
            let interaction_boundary = now
                - chrono::Duration::seconds(
                    proactive_config.recent_interaction_cooldown_secs() as i64
                );
            if profile
                .last_private_interaction
                .is_some_and(|last_private| last_private > interaction_boundary)
            {
                return false;
            }
        }

        true
    }

    /// 持久化状态优先，本地记忆只用于兼容刚升级的旧部署。
    async fn should_decide_main_admin_chat(&self, main_admin: i64) -> bool {
        let proactive_config = crate::config::get().proactive().clone();
        let decision_key = main_admin_state_key(main_admin);
        let decision_boundary = Local::now()
            - chrono::Duration::seconds(proactive_config.main_admin_decision_interval_secs() as i64);
        if self
            .memory_manager
            .get_proactive_state(&decision_key)
            .await
            .and_then(|state| state.last_decision_at)
            .is_some_and(|last_decision| last_decision > decision_boundary)
        {
            return false;
        }
        let recent_memories = self
            .memory_manager
            .get_recent_memories_for_subject(main_admin, Some("proactive_main_admin_decision"), 1)
            .await;
        !recent_memories.iter().any(|memory| {
            memory.subject_id == Some(main_admin)
                && memory.context == "proactive_main_admin_decision"
                && memory.timestamp > decision_boundary
        })
    }

    async fn get_active_groups(&self) -> Vec<i64> {
        let now = Local::now();
        let one_day_ago = now - chrono::Duration::days(1);

        let candidates = self
            .memory_manager
            .get_proactive_group_candidates(one_day_ago, 32)
            .await;
        let mut authorized = Vec::with_capacity(candidates.len());
        for profile in candidates {
            if group_access::is_authorized_group(profile.group_id)
                .await
                .unwrap_or(false)
            {
                authorized.push(profile.group_id);
            }
        }
        authorized
    }

    async fn get_active_users(&self) -> Vec<i64> {
        let now = Local::now();
        let three_days_ago = now - chrono::Duration::days(3);
        let main_admin = configured_owner_target().await;

        self.memory_manager
            .get_proactive_user_candidates(three_days_ago, main_admin, 32)
            .await
            .into_iter()
            .map(|profile| profile.user_id)
            .collect()
    }

    async fn select_chat_target(&self, groups: Vec<i64>, users: Vec<i64>) -> ChatTarget {
        let personality = self.memory_manager.get_bot_personality().await;

        let mut targets = Vec::new();
        if personality.social_confidence >= 5 {
            targets.extend(groups.into_iter().map(ChatTarget::Group));
        }
        targets.extend(users.into_iter().map(ChatTarget::User));

        targets
            .choose(&mut rand::rng())
            .cloned()
            .unwrap_or(ChatTarget::None)
    }

    async fn initiate_group_chat(&self, group_id: i64) -> Result<()> {
        // Profile persistence can outlive an authorization revocation. Check the
        // current allowlist before doing any model work and again before send.
        if !group_access::is_authorized_group(group_id)
            .await
            .unwrap_or(false)
        {
            return Ok(());
        }
        if !self.can_send_to_target("group", group_id).await {
            return Ok(());
        }
        // 检查是否应该在这个群组发起对话
        if !self
            .topic_generator
            .should_initiate_conversation(Some(group_id), None)
            .await
        {
            return Ok(());
        }

        // 生成话题
        if let Some(topic) = self
            .topic_generator
            .generate_memory_based_topic(Some(group_id), None)
            .await?
        {
            let content = topic.content.clone();

            // 发送消息
            if !group_access::is_authorized_group(group_id)
                .await
                .unwrap_or(false)
                || !self.can_send_regular_chat().await
                || !self.can_send_to_target("group", group_id).await
            {
                return Ok(());
            }
            let grace =
                prepared_grace_duration(crate::config::get().proactive().prepared_grace_ms());
            let send_result = send_tracked_message_with_revalidation(
                &self.bot,
                MessageDestination::Group(group_id),
                Message::from(content.clone()),
                OutgoingSource::Proactive,
                None,
                || async {
                    if !grace.is_zero() {
                        sleep(grace).await;
                    }
                    self.can_send_regular_chat().await
                        && self.can_send_to_target("group", group_id).await
                },
            )
            .await;
            if let Err(error) = send_result {
                if matches!(
                    error,
                    TrackedSendError::InvalidTarget | TrackedSendError::Transport(_)
                ) {
                    eprintln!(
                        "[ERROR] 主动群聊消息发送失败 (群组: {}): {}",
                        group_id, error
                    );
                }
                return Ok(());
            }

            let target_key = target_state_key("group", group_id);
            self.memory_manager
                .record_proactive_event(
                    None,
                    &[GLOBAL_PROACTIVE_STATE_KEY.to_string(), target_key],
                    Local::now(),
                )
                .await?;

            // 记录这次主动对话
            self.memory_manager
                .add_conversation_memory(
                    group_id,
                    &format!("主动发起话题: {}", content),
                    "proactive_group_chat",
                )
                .await?;
        }

        Ok(())
    }

    async fn initiate_private_chat(&self, user_id: i64) -> Result<bool> {
        if !self.can_send_to_target("private", user_id).await {
            return Ok(false);
        }
        // 检查是否应该向这个用户发起对话
        if !self
            .topic_generator
            .should_initiate_conversation(None, Some(user_id))
            .await
        {
            return Ok(false);
        }

        let Some(intent) = self.generate_private_reach_out(user_id).await? else {
            return Ok(false);
        };
        let content = intent.message().as_text().to_string();

        if !self.can_send_regular_chat().await
            || !self.can_send_to_target("private", user_id).await
            || !self.deliver_private_reach_out(user_id, &intent).await
        {
            return Ok(false);
        }

        let target_key = target_state_key("private", user_id);
        self.memory_manager
            .record_proactive_event(
                None,
                &[GLOBAL_PROACTIVE_STATE_KEY.to_string(), target_key],
                Local::now(),
            )
            .await?;
        self.memory_manager
            .add_conversation_memory(
                user_id,
                &format!("主动发起话题: {}", content),
                "proactive_private_chat",
            )
            .await?;
        Ok(true)
    }

    async fn can_send_to_target(&self, scope: &str, subject_id: i64) -> bool {
        let proactive_config = crate::config::get().proactive().clone();
        let now = Local::now();
        let boundary =
            now - chrono::Duration::seconds(proactive_config.target_cooldown_secs() as i64);
        let state_key = target_state_key(scope, subject_id);
        if self
            .memory_manager
            .get_proactive_state(&state_key)
            .await
            .and_then(|state| state.last_sent_at)
            .is_some_and(|last_sent| last_sent > boundary)
        {
            return false;
        }

        let interaction_boundary = now
            - chrono::Duration::seconds(proactive_config.recent_interaction_cooldown_secs() as i64);
        if scope == "private"
            && self
                .memory_manager
                .get_user_profile(subject_id)
                .await
                .and_then(|profile| profile.last_private_interaction)
                .is_some_and(|last_private| last_private > interaction_boundary)
        {
            return false;
        }
        if scope == "group"
            && self
                .memory_manager
                .get_group_profile(subject_id)
                .await
                .is_some_and(|profile| profile.last_activity > interaction_boundary)
        {
            return false;
        }
        true
    }

    pub async fn handle_user_response(
        &self,
        user_id: i64,
        message: &str,
        _is_group: bool,
    ) -> Result<()> {
        let context = if _is_group {
            "group_chat"
        } else {
            "private_chat"
        };
        let understanding = understand(UnderstandingRequest::text(message, context)).await;

        // 更新用户档案
        self.update_user_profile(user_id, _is_group, &understanding)
            .await?;

        // 分析情绪变化
        MOOD_SYSTEM
            .analyze_and_update_mood_for_subject_with_understanding(
                message,
                context,
                Some(user_id),
                &understanding,
            )
            .await?;

        // 记录对话记忆
        let memory_tags = understanding.memory_tags();
        self.memory_manager
            .add_conversation_memory_with_hints(
                user_id,
                message,
                context,
                Some(understanding.memory_importance()),
                &memory_tags,
            )
            .await?;

        Ok(())
    }

    async fn update_user_profile(
        &self,
        user_id: i64,
        _is_group: bool,
        understanding: &MessageUnderstanding,
    ) -> Result<()> {
        let now = Local::now();
        let gratitude = understanding.gratitude;
        let interests = understanding.interests.clone();
        self.memory_manager
            .mutate_user_profile(user_id, move |current| {
                let mut profile = current.unwrap_or_else(|| crate::memory::UserProfile {
                    user_id,
                    nickname: "未设置昵称".to_string(),
                    personality_traits: Vec::new(),
                    interests: Vec::new(),
                    relationship_level: 1,
                    last_interaction: now,
                    interaction_count: 0,
                    last_private_interaction: if _is_group { None } else { Some(now) },
                    mood_history: Vec::new(),
                });
                profile.last_interaction = now;
                profile.interaction_count = profile.interaction_count.saturating_add(1);
                if !_is_group {
                    profile.last_private_interaction = Some(now);
                }
                if gratitude {
                    profile.relationship_level = (profile.relationship_level + 1).min(10);
                }
                for interest in interests {
                    if !profile.interests.contains(&interest) {
                        profile.interests.push(interest);
                    }
                }
                profile.interests.truncate(20);
                profile
            })
            .await?;

        Ok(())
    }
}

#[cfg(test)]
fn is_sent_proactive_context(context: &str) -> bool {
    matches!(context, "proactive_group_chat" | "proactive_private_chat")
}

#[derive(Debug, Clone)]
enum ChatTarget {
    Group(i64),
    User(i64),
    None,
}

#[cfg(test)]
mod tests {
    use super::{is_sent_proactive_context, prepared_grace_duration};
    use std::time::Duration;

    #[test]
    fn skipped_main_admin_decision_does_not_start_global_cooldown() {
        assert!(!is_sent_proactive_context("proactive_main_admin_decision"));
        assert!(is_sent_proactive_context("proactive_private_chat"));
    }

    #[test]
    fn proactive_prepared_grace_is_disabled_or_tightly_bounded() {
        assert_eq!(prepared_grace_duration(0), Duration::ZERO);
        assert_eq!(prepared_grace_duration(1), Duration::from_millis(300));
        assert_eq!(prepared_grace_duration(500), Duration::from_millis(500));
        assert_eq!(prepared_grace_duration(5_000), Duration::from_millis(1_000));
    }
}
