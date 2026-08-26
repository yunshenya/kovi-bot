use anyhow::Result;
use serde::{Deserialize, Serialize};
use yunxi_core::{MindInfluenceMode, MindSnapshotLimits};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MindConfig {
    enabled: bool,
    belief_enabled: bool,
    preference_enabled: bool,
    interest_enabled: bool,
    curiosity_enabled: bool,
    agenda_enabled: bool,
    reflection_enabled: bool,
    mind_planner_enabled: bool,
    influence_mode: MindInfluenceMode,
    snapshot_timeout_ms: u64,
    event_update_timeout_ms: u64,
    max_relevant_beliefs: usize,
    max_relevant_preferences: usize,
    max_relevant_interests: usize,
    max_open_questions: usize,
    max_agenda_items: usize,
    max_learned_beliefs_per_scope: usize,
    max_preferences: usize,
    max_interests: usize,
    max_open_questions_per_scope: usize,
    max_curiosity_per_person: usize,
    max_global_agenda: usize,
    max_agenda_per_person: usize,
    max_agenda_per_conversation: usize,
    agenda_half_life_hours: u64,
    question_cooldown_minutes: u64,
    conversation_end_idle_minutes: u64,
    reflection_min_interval_minutes: u64,
    reflection_max_events: usize,
}

impl Default for MindConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            belief_enabled: true,
            preference_enabled: true,
            interest_enabled: true,
            curiosity_enabled: true,
            agenda_enabled: true,
            reflection_enabled: true,
            mind_planner_enabled: true,
            influence_mode: MindInfluenceMode::Shadow,
            snapshot_timeout_ms: 75,
            event_update_timeout_ms: 40,
            max_relevant_beliefs: 8,
            max_relevant_preferences: 8,
            max_relevant_interests: 8,
            max_open_questions: 6,
            max_agenda_items: 8,
            max_learned_beliefs_per_scope: 64,
            max_preferences: 32,
            max_interests: 32,
            max_open_questions_per_scope: 8,
            max_curiosity_per_person: 8,
            max_global_agenda: 24,
            max_agenda_per_person: 12,
            max_agenda_per_conversation: 12,
            agenda_half_life_hours: 24,
            question_cooldown_minutes: 120,
            conversation_end_idle_minutes: 10,
            reflection_min_interval_minutes: 60,
            reflection_max_events: 32,
        }
    }
}

impl MindConfig {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            (1..=5_000).contains(&self.snapshot_timeout_ms),
            "mind.snapshot_timeout_ms 必须在 1..=5000"
        );
        anyhow::ensure!(
            (1..=500).contains(&self.event_update_timeout_ms),
            "mind.event_update_timeout_ms 必须在 1..=500"
        );
        self.snapshot_limits()
            .validate()
            .map_err(anyhow::Error::from)?;
        for (name, value) in [
            (
                "max_learned_beliefs_per_scope",
                self.max_learned_beliefs_per_scope,
            ),
            ("max_preferences", self.max_preferences),
            ("max_interests", self.max_interests),
            (
                "max_open_questions_per_scope",
                self.max_open_questions_per_scope,
            ),
            ("max_curiosity_per_person", self.max_curiosity_per_person),
            ("max_global_agenda", self.max_global_agenda),
            ("max_agenda_per_person", self.max_agenda_per_person),
            (
                "max_agenda_per_conversation",
                self.max_agenda_per_conversation,
            ),
        ] {
            anyhow::ensure!((1..=128).contains(&value), "mind.{name} 必须在 1..=128");
        }
        anyhow::ensure!(
            (1..=24 * 30).contains(&self.agenda_half_life_hours),
            "mind.agenda_half_life_hours 必须在 1..=720"
        );
        anyhow::ensure!(
            (1..=24 * 60).contains(&self.question_cooldown_minutes),
            "mind.question_cooldown_minutes 必须在 1..=1440"
        );
        anyhow::ensure!(
            (1..=24 * 60).contains(&self.conversation_end_idle_minutes),
            "mind.conversation_end_idle_minutes 必须在 1..=1440"
        );
        anyhow::ensure!(
            (1..=24 * 60).contains(&self.reflection_min_interval_minutes),
            "mind.reflection_min_interval_minutes 必须在 1..=1440"
        );
        anyhow::ensure!(
            (1..=32).contains(&self.reflection_max_events),
            "mind.reflection_max_events 必须在 1..=32"
        );
        Ok(())
    }

    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    pub const fn belief_enabled(&self) -> bool {
        self.enabled && self.belief_enabled
    }

    pub const fn preference_enabled(&self) -> bool {
        self.enabled && self.preference_enabled
    }

    pub const fn interest_enabled(&self) -> bool {
        self.enabled && self.interest_enabled
    }

    pub const fn curiosity_enabled(&self) -> bool {
        self.enabled && self.curiosity_enabled
    }

    pub const fn agenda_enabled(&self) -> bool {
        self.enabled && self.agenda_enabled
    }

    pub const fn reflection_enabled(&self) -> bool {
        self.enabled && self.reflection_enabled
    }

    pub const fn mind_planner_enabled(&self) -> bool {
        self.enabled && self.mind_planner_enabled
    }

    pub const fn influence_mode(&self) -> MindInfluenceMode {
        if self.enabled {
            self.influence_mode
        } else {
            MindInfluenceMode::Disabled
        }
    }

    pub const fn snapshot_timeout_ms(&self) -> u64 {
        self.snapshot_timeout_ms
    }

    pub const fn event_update_timeout_ms(&self) -> u64 {
        self.event_update_timeout_ms
    }

    pub const fn snapshot_limits(&self) -> MindSnapshotLimits {
        MindSnapshotLimits {
            beliefs: self.max_relevant_beliefs,
            preferences: self.max_relevant_preferences,
            interests: self.max_relevant_interests,
            open_questions: self.max_open_questions,
            agenda_items: self.max_agenda_items,
        }
    }

    pub const fn reflection_min_interval_minutes(&self) -> u64 {
        self.reflection_min_interval_minutes
    }

    pub const fn reflection_max_events(&self) -> usize {
        self.reflection_max_events
    }

    pub const fn max_learned_beliefs_per_scope(&self) -> usize {
        self.max_learned_beliefs_per_scope
    }

    pub const fn max_preferences(&self) -> usize {
        self.max_preferences
    }

    pub const fn max_interests(&self) -> usize {
        self.max_interests
    }

    pub const fn max_open_questions_per_scope(&self) -> usize {
        self.max_open_questions_per_scope
    }

    pub const fn max_curiosity_per_person(&self) -> usize {
        self.max_curiosity_per_person
    }

    pub const fn max_agenda_for_scope(&self, scope: yunxi_core::MindScope) -> usize {
        match scope {
            yunxi_core::MindScope::Global => self.max_global_agenda,
            yunxi_core::MindScope::Person { .. } => self.max_agenda_per_person,
            yunxi_core::MindScope::Conversation { .. } => self.max_agenda_per_conversation,
        }
    }

    pub const fn agenda_half_life_hours(&self) -> u64 {
        self.agenda_half_life_hours
    }

    pub const fn question_cooldown_minutes(&self) -> u64 {
        self.question_cooldown_minutes
    }

    pub const fn conversation_end_idle_minutes(&self) -> u64 {
        self.conversation_end_idle_minutes
    }
}
