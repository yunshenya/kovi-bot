//! Best-effort projection of legacy Kovi activity into Yunxi Core events.

use crate::model::MessageDestination;
use chrono::Utc;
use kovi::tokio::time::timeout;
use std::time::Duration;
use yunxi_core::{
    Admission, EventPriority, EventScope, GoalCompletedEvent, GoalState, GoalUpdatedEvent,
    IdentityStore, InteractionCues, InteractionCuesObservedEvent, WorldEvent, WorldEventKind,
};

const RELIABLE_EVENT_TIMEOUT: Duration = Duration::from_millis(250);
const NORMAL_EVENT_TIMEOUT: Duration = Duration::from_millis(250);
const AGENT_GOAL_TIMEOUT: Duration = Duration::from_secs(1);
const AGENT_TASK_SOURCE_KIND: &str = "kovi_agent_task";

/// Project an event associated with an existing QQ destination. Identity and
/// admission failures are observations only: legacy execution remains the
/// source of truth throughout this migration phase.
pub(crate) async fn project_destination(
    destination: MessageDestination,
    priority: EventPriority,
    kind: WorldEventKind,
) {
    let wait = projection_timeout(priority);
    let Some(bridge) = super::CORE_BRIDGE.get() else {
        kovi::log::warn!("Yunxi event projection failed: Core bridge is not installed");
        return;
    };
    match timeout(
        wait,
        bridge.project_destination(destination, priority, kind),
    )
    .await
    {
        Ok(Ok(Admission::Accepted)) => {}
        Ok(Ok(Admission::DroppedAtCapacity)) => {
            kovi::log::warn!("Yunxi event projection dropped at runtime capacity");
        }
        Ok(Err(error)) => kovi::log::warn!("Yunxi event projection failed: {error}"),
        Err(_) => kovi::log::warn!("Yunxi event projection timed out"),
    }
}

/// Project semantic evidence already produced by a legacy handler. The
/// conversion into [`InteractionCues`] happens at the semantic boundary; this
/// function only resolves the canonical Person and admits a bounded event.
/// Core 接管的一条消息：补跑一次会话理解，把结果喂给情绪、关系与画像。
///
/// 为什么需要它：`understand()` 一直只挂在 Host 的两个入站处理器上，而普通私聊
/// 文本与"指向她的群消息"都归 Core——那些回合于是既没有情绪、也没有兴趣/性格/
/// 关系等级的更新。理解层是这条链路唯一的生产者，缺了它，`project_interaction_cues`
/// 与画像学习对 Core 流量就是死代码。
///
/// 三条边界：
/// - **只补理解，不参与回复**：结果只走 `project_interaction_cues`（关系/情绪）与
///   `learn_user_profile_from_message`（画像）；这一轮回不回、回什么完全不受影响。
/// - **后台 + 超时**：宿主不等待它，超时/失败只记日志；它也不改变消息去向。
/// - **只对 Core 接管的回合跑**：Host 那两条路已经跑过一次，重复调用会让
///   `interaction_count` 涨两次。
pub(crate) fn observe_core_owned_message(
    user_id: i64,
    text: String,
    context: &'static str,
    nickname: String,
    is_private: bool,
    visible_reply_allowed: bool,
) {
    if !should_observe_core_owned(
        visible_reply_allowed,
        &text,
        crate::config::get().understanding().core_owned_enabled(),
    ) {
        return;
    }
    kovi::tokio::spawn(async move {
        let request = crate::model::UnderstandingRequest::text(&text, context);
        let understanding = match timeout(
            CORE_UNDERSTANDING_TIMEOUT,
            crate::model::understand(request),
        )
        .await
        {
            Ok(understanding) => understanding,
            Err(_) => {
                kovi::log::warn!(
                    "Yunxi core-owned understanding timed out (user: {user_id}, context: {context})"
                );
                return;
            }
        };
        project_interaction_cues(user_id, understanding.interaction_cues());
        crate::model::learn_user_profile_from_message(
            user_id,
            &text,
            &nickname,
            is_private,
            &understanding,
        )
        .await;
    });
}

/// 这条补跑的理解最多花多久。它纯属观测，宁可不记也不让任务堆积。
const CORE_UNDERSTANDING_TIMEOUT: Duration = Duration::from_secs(15);

/// 要不要为这条消息补跑一次会话理解。
///
/// 三个条件缺一不可：开关开着、正文不是空的、这一条是 **Core 的可见回合**
/// （`visible_reply_allowed`）。
///
/// 最后一条是"恰好一次"的关键：观察型消息（未点名的群聊背景流量）由 Host 处理，
/// 而那两条入站处理器自己会跑一次同样的理解——在这里再跑一次，同一个人的
/// `interaction_count` 就会涨两次、关系等级跟着多涨一级。
pub(crate) fn should_observe_core_owned(
    visible_reply_allowed: bool,
    text: &str,
    enabled: bool,
) -> bool {
    visible_reply_allowed && enabled && !text.trim().is_empty()
}

pub(crate) fn project_interaction_cues(user_id: i64, cues: InteractionCues) {
    if !has_interaction_evidence(cues) {
        return;
    }
    kovi::tokio::spawn(async move {
        match timeout(
            NORMAL_EVENT_TIMEOUT,
            project_interaction_cues_inner(user_id, cues),
        )
        .await
        {
            Ok(Ok(Admission::Accepted)) => {}
            Ok(Ok(Admission::DroppedAtCapacity)) => {
                kovi::log::warn!("Yunxi interaction-cue projection dropped at runtime capacity");
            }
            Ok(Err(error)) => {
                kovi::log::warn!("Yunxi interaction-cue projection failed: {error}");
            }
            Err(_) => kovi::log::warn!("Yunxi interaction-cue projection timed out"),
        }
    });
}

fn has_interaction_evidence(cues: InteractionCues) -> bool {
    cues != InteractionCues::default()
}

async fn project_interaction_cues_inner(
    user_id: i64,
    cues: InteractionCues,
) -> Result<Admission, String> {
    let identities = super::IDENTITY_STORE
        .get()
        .ok_or_else(|| "identity store is not installed".to_string())?;
    let bridge = super::CORE_BRIDGE
        .get()
        .ok_or_else(|| "Core bridge is not installed".to_string())?;
    let external = super::qq::person(user_id).map_err(|error| error.to_string())?;
    let person_id = identities
        .resolve_external_identity(&external)
        .await
        .map_err(|error| error.to_string())?;
    if let Some(mind) = super::mind_runtime() {
        let wait = Duration::from_millis(mind.config().event_update_timeout_ms());
        match timeout(wait, mind.observe_interaction_cues(person_id, cues)).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                kovi::log::warn!("Yunxi Mind interaction-cue update failed soft: {error}");
            }
            Err(_) => {
                kovi::log::warn!("Yunxi Mind interaction-cue update timed out and failed soft");
            }
        }
    }
    record_gratitude_evidence(person_id, cues).await;
    let observed =
        InteractionCuesObservedEvent::new(person_id, cues).map_err(|error| error.to_string())?;
    bridge
        .submit_event(WorldEvent::new(
            Utc::now(),
            EventScope::Person { person_id },
            EventPriority::Normal,
            WorldEventKind::InteractionCuesObserved(observed),
        ))
        .await
        .map_err(|error| error.to_string())
}

/// 语义层的 `gratitude`（明确的谢意、歉意、关心）折算成相处证据强度的系数。
///
/// 它给的是一条**中等偏强**的善意：`gratitude_strength = 0.75`（语义层的固定
/// 取值）乘这个系数约等于 0.5 的相处证据，于是约 31 次明确道谢把好感推过 0.5。
/// 比逐条判定的"顺带的好语气"更强是应该的——它判的是"她在道谢"，不是"这句话
/// 语气不错"。
const GRATITUDE_EVIDENCE_SCALE: f32 = 0.65;

/// 道谢走**证据通道**，而不是回合收尾的整行回写。
///
/// 关系行只有一根 `updated_at` 时钟，五个维度各有写者：整行回写写回的是**回合开始
/// 时的快照**，它在回合进行中到账的证据上盖过去（2026-09-14 张力事故）。好感与信任
/// 因此和张力一样，只能由这条 delta 通道改：它把漂移与本列的变化放进同一条 SQL，
/// 既不丢别列的衰减，也不会被别处的回写抹掉。
async fn record_gratitude_evidence(person_id: yunxi_core::PersonId, cues: InteractionCues) {
    let gratitude = cues.gratitude_strength;
    if !gratitude.is_finite() || gratitude <= 0.0 {
        return;
    }
    let Some(relations) = super::relation_store() else {
        return;
    };
    // 负号 = 友好：Core 的相处证据约定"正 = 不友好"。
    let strength = -(gratitude * GRATITUDE_EVIDENCE_SCALE).clamp(0.0, 1.0);
    match relations
        .nudge(person_id, yunxi_core::relation_evidence_nudge(strength))
        .await
    {
        Ok(Some(state)) => println!(
            "[RELATION] 道谢已记账 person={person_id} strength={strength:+.2} tension={:.3} affinity={:.3} trust={:.3}",
            state.tension, state.affinity, state.trust
        ),
        Ok(None) => println!(
            "[RELATION] 道谢跳过：该 person 还没有关系行 person={person_id} strength={strength:+.2}"
        ),
        Err(error) => kovi::log::warn!("Yunxi gratitude evidence failed soft: {error}"),
    }
}

pub(crate) fn project_agent_task(
    task_id: i64,
    actor_user_id: i64,
    question: &str,
    target: GoalState,
) {
    let question = question.to_owned();
    kovi::tokio::spawn(async move {
        match timeout(
            AGENT_GOAL_TIMEOUT,
            project_agent_task_inner(task_id, actor_user_id, &question, target),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => kovi::log::warn!("Yunxi agent-task goal projection failed: {error}"),
            Err(_) => kovi::log::warn!("Yunxi agent-task goal projection timed out"),
        }
    });
}

async fn project_agent_task_inner(
    task_id: i64,
    actor_user_id: i64,
    question: &str,
    target: GoalState,
) -> Result<(), String> {
    if task_id <= 0 {
        return Err("agent task id is invalid".to_string());
    }
    let identities = super::IDENTITY_STORE
        .get()
        .ok_or_else(|| "identity store is not installed".to_string())?;
    let goals = super::GOAL_STORE
        .get()
        .ok_or_else(|| "goal store is not installed".to_string())?;
    let bridge = super::CORE_BRIDGE
        .get()
        .ok_or_else(|| "Core bridge is not installed".to_string())?;
    let external = super::qq::person(actor_user_id).map_err(|error| error.to_string())?;
    let person_id = identities
        .resolve_external_identity(&external)
        .await
        .map_err(|error| error.to_string())?;
    let source_key = task_id.to_string();
    let mut goal = goals
        .get_or_create_external_person_goal(
            AGENT_TASK_SOURCE_KIND,
            &source_key,
            person_id,
            question,
        )
        .await
        .map_err(|error| error.to_string())?;
    if target != GoalState::Active {
        goal = goals
            .transition_external_goal(AGENT_TASK_SOURCE_KIND, &source_key, target)
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "agent-task goal link disappeared".to_string())?;
    }
    let kind = if goal.state() == GoalState::Completed {
        WorldEventKind::GoalCompleted(GoalCompletedEvent { goal_id: goal.id() })
    } else {
        WorldEventKind::GoalUpdated(GoalUpdatedEvent { goal_id: goal.id() })
    };
    bridge
        .submit_event(WorldEvent::new(
            Utc::now(),
            EventScope::Goal { goal_id: goal.id() },
            EventPriority::High,
            kind,
        ))
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
}

const fn projection_timeout(priority: EventPriority) -> Duration {
    if priority.requires_backpressure() {
        RELIABLE_EVENT_TIMEOUT
    } else {
        NORMAL_EVENT_TIMEOUT
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use yunxi_core::{ReminderDueEvent, ToolCompletedEvent};

    #[test]
    fn core_owned_understanding_runs_exactly_once_per_visible_turn() {
        // 三个条件：是 Core 的可见回合、开关开着、正文里有东西。
        assert!(should_observe_core_owned(true, "在吗", true));
        // 观察型消息（未点名的群聊背景流量）由 Host 处理，Host 的两条入站处理器
        // 自己会跑一次理解——这里必须排掉，否则 interaction_count 涨两次。
        assert!(!should_observe_core_owned(false, "在吗", true));
        // 开关关掉即回到原状。
        assert!(!should_observe_core_owned(true, "在吗", false));
        // 空正文不值得为它花一次分类调用（图片、贴纸这类消息正文就是空的）。
        assert!(!should_observe_core_owned(true, "", true));
        assert!(!should_observe_core_owned(true, "   \n\t ", true));
    }

    #[test]
    fn projected_payloads_remain_bounded_and_valid() {
        let reminder = WorldEvent::new(
            Utc::now(),
            EventScope::Global,
            EventPriority::High,
            WorldEventKind::ReminderDue(ReminderDueEvent {
                reference: "reminder:42".to_string(),
            }),
        );
        let tool = WorldEvent::new(
            Utc::now(),
            EventScope::Global,
            EventPriority::Normal,
            WorldEventKind::ToolCompleted(ToolCompletedEvent {
                operation: "weather.current".to_string(),
                output: String::new(),
                requires_follow_up: false,
            }),
        );

        assert!(reminder.validate(8).is_ok());
        assert!(tool.validate(8).is_ok());
    }

    #[test]
    fn confident_neutral_sentiment_is_still_meaningful_evidence() {
        assert!(!has_interaction_evidence(InteractionCues::default()));
        assert!(has_interaction_evidence(InteractionCues {
            sentiment_confidence: 0.9,
            ..InteractionCues::default()
        }));
    }
}
