//! Host-side projection into the platform-neutral Core proactive model.
//!
//! Legacy memory/profile records still use QQ subject IDs during migration.
//! This adapter bounds and summarizes those records, then drops the external ID
//! before invoking Core.

use crate::memory::{BotPersonality, MemoryEntry, MemoryType, UserProfile};
use chrono::{DateTime, Local};
use yunxi_core::{
    PersonId, ProactiveCandidate, ProactiveContext, ProactiveDecision, ProactiveOpportunity,
    ProactiveSystem, ProactiveValidationError,
};

const MAX_PROJECTED_MEMORIES: usize = 16;
const MINIMUM_SALIENCE: u8 = 45;

pub(crate) fn project_private_opportunity(
    person_id: PersonId,
    profile: &UserProfile,
    personality: &BotPersonality,
    memories: &[MemoryEntry],
    now: DateTime<Local>,
    system_load: u8,
) -> Result<Option<ProactiveOpportunity>, ProactiveValidationError> {
    let signals = project_signals(memories, now);
    let idle_hours = profile
        .last_private_interaction
        .map(|last| now.signed_duration_since(last).num_hours().max(0))
        .unwrap_or(0)
        .min(i64::from(u16::MAX)) as u16;
    let candidate = ProactiveCandidate::new(
        person_id,
        profile.relationship_level.min(10) * 10,
        personality.curiosity_level.min(10) * 10,
        signals.memory_salience,
        signals.recent_event_salience,
        idle_hours,
    )?;
    let social_energy = ((u16::from(personality.energy_level.min(10))
        + u16::from(personality.social_confidence.min(10)))
        * 5) as u8;
    let context = ProactiveContext::new(social_energy, system_load, MINIMUM_SALIENCE)?;

    match ProactiveSystem.decide(context, &[candidate])? {
        ProactiveDecision::ReachOut(opportunity) => Ok(Some(opportunity)),
        ProactiveDecision::Silent(_) => Ok(None),
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ProjectedSignals {
    memory_salience: u8,
    recent_event_salience: u8,
}

fn project_signals(memories: &[MemoryEntry], now: DateTime<Local>) -> ProjectedSignals {
    let mut signals = ProjectedSignals::default();
    for memory in memories
        .iter()
        .take(MAX_PROJECTED_MEMORIES)
        .filter(|memory| is_grounded_memory(memory))
    {
        let importance = memory.importance.min(10) * 10;
        signals.memory_salience = signals.memory_salience.max(importance);
        let age_hours = now
            .signed_duration_since(memory.timestamp)
            .num_hours()
            .max(0);
        if age_hours <= 72 {
            let type_bonus = u8::from(matches!(memory.memory_type, MemoryType::Event)) * 20;
            signals.recent_event_salience = signals
                .recent_event_salience
                .max(importance.saturating_add(type_bonus).min(100));
        }
    }
    signals
}

fn is_grounded_memory(memory: &MemoryEntry) -> bool {
    if memory.context.starts_with("proactive_")
        || !matches!(
            memory.memory_type,
            MemoryType::Conversation | MemoryType::Event
        )
    {
        return false;
    }
    let content = memory.content.trim();
    content.chars().count() >= 8 && !content.starts_with("芸汐:") && !content.starts_with("芸汐：")
}

#[cfg(test)]
mod tests {
    use super::project_private_opportunity;
    use crate::memory::{BotPersonality, MemoryEntry, MemoryType, UserProfile};
    use chrono::{Duration, Local};
    use yunxi_core::{PersonId, ProactiveMotive};

    fn personality() -> BotPersonality {
        BotPersonality {
            current_mood: "neutral".to_string(),
            mood_intensity: 5,
            energy_level: 7,
            social_confidence: 7,
            curiosity_level: 8,
            last_mood_change: Local::now(),
            personality_traits: Vec::new(),
        }
    }

    fn profile(now: chrono::DateTime<Local>) -> UserProfile {
        UserProfile {
            user_id: 42,
            nickname: "test".to_string(),
            personality_traits: Vec::new(),
            interests: Vec::new(),
            relationship_level: 7,
            last_interaction: now - Duration::hours(24),
            interaction_count: 10,
            last_private_interaction: Some(now - Duration::hours(24)),
            mood_history: Vec::new(),
        }
    }

    fn memory(now: chrono::DateTime<Local>, content: &str, importance: u8) -> MemoryEntry {
        MemoryEntry {
            id: "memory".to_string(),
            content: content.to_string(),
            timestamp: now - Duration::hours(24),
            memory_type: MemoryType::Conversation,
            importance,
            tags: Vec::new(),
            context: "private_chat".to_string(),
            subject_id: Some(42),
        }
    }

    #[test]
    fn natural_language_content_does_not_override_structured_signal_type() {
        let now = Local::now();
        let opportunity = project_private_opportunity(
            PersonId::new(),
            &profile(now),
            &personality(),
            &[memory(now, "这两天工作太累了，有点忙不过来", 5)],
            now,
            0,
        )
        .expect("valid projection")
        .expect("grounded opportunity");
        assert_eq!(opportunity.motive(), ProactiveMotive::Share);
    }

    #[test]
    fn profile_without_grounded_memory_stays_silent() {
        let now = Local::now();
        assert!(
            project_private_opportunity(
                PersonId::new(),
                &profile(now),
                &personality(),
                &[],
                now,
                0,
            )
            .expect("valid projection")
            .is_none()
        );
    }
}
