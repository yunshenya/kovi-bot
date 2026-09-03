use super::{
    AgendaItemId, AgendaItemKind, InterestId, MindInfluenceMode, MindReasonTag, OpenQuestionId,
};
use crate::{DecisionDisposition, PlannerInput, WorldEventKind};
use serde::{Deserialize, Serialize};

const MIN_QUESTION_SALIENCE: f32 = 0.65;
const MIN_AGENDA_RESUME_SCORE: f32 = 0.62;
const MIN_DUE_AGENDA_SCORE: f32 = 0.45;
const MIN_INTEREST_ACTIVATION: f32 = 0.72;
const MIN_INTEREST_AFFINITY: f32 = 0.45;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "id", rename_all = "snake_case")]
pub enum MindDecisionReference {
    Agenda(AgendaItemId),
    OpenQuestion(OpenQuestionId),
    Interest(InterestId),
}

/// Deterministic, bounded projection of a Mind snapshot onto the existing V1
/// disposition vocabulary. It never creates an action and is therefore safe
/// to run in both Shadow and Active modes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MindDecisionProjection {
    disposition: DecisionDisposition,
    baseline: DecisionDisposition,
    reason_tags: Vec<MindReasonTag>,
    reference: Option<MindDecisionReference>,
    would_disagree: bool,
    /// Propositions of the high-confidence, stable beliefs that the current
    /// inbound message explicitly opposes (bounded). Surfaced to the host so
    /// the reply can genuinely acknowledge the disagreement instead of the
    /// detection being recorded and then ignored.
    belief_conflicts: Vec<String>,
}

impl MindDecisionProjection {
    #[must_use]
    pub fn for_input(input: &PlannerInput, baseline: DecisionDisposition) -> Self {
        let mut projection = Self {
            disposition: baseline,
            baseline,
            reason_tags: Vec::new(),
            reference: None,
            would_disagree: false,
            belief_conflicts: Vec::new(),
        };
        if input.mind.is_empty() || input.mind.influence_mode() == MindInfluenceMode::Disabled {
            return projection;
        }
        let conflicting_beliefs: Vec<String> = match input.event.kind() {
            WorldEventKind::MessageReceived(message)
                if !message.stop_requested && message.visible_reply_allowed =>
            {
                input
                    .mind
                    .beliefs()
                    .iter()
                    .filter(|belief| belief.confidence >= 0.7 && belief.stability >= 0.5)
                    .filter(|belief| {
                        super::relevance::explicitly_opposes(
                            &belief.proposition,
                            message.content.as_text(),
                        )
                    })
                    .take(3)
                    .map(|belief| belief.proposition.clone())
                    .collect()
            }
            _ => Vec::new(),
        };
        projection.belief_conflicts = conflicting_beliefs;
        projection.would_disagree = !projection.belief_conflicts.is_empty();
        if projection.would_disagree {
            projection.push_reason(MindReasonTag::BeliefConflict);
        }

        match input.event.kind() {
            WorldEventKind::MessageReceived(message) => {
                if message.stop_requested || !message.visible_reply_allowed {
                    projection.disposition = DecisionDisposition::Silent;
                    return projection;
                }
                if message.content.as_text().trim_start().starts_with('#') {
                    projection.disposition = DecisionDisposition::Silent;
                    return projection;
                }
                if message.conversation_kind == crate::ConversationKind::Group
                    && !message.addressed_to_agent
                    && !message.replies_to_agent
                    && !message.explicit_request
                {
                    projection.push_reason(MindReasonTag::LowSocialValue);
                }

                // A clear current-turn request outranks optional long-lived
                // agenda. Beliefs can still shape the angle of the reply.
                if message.explicit_request
                    || message.addressed_to_agent
                    || message.replies_to_agent
                {
                    return projection;
                }

                if let Some(question) = input
                    .mind
                    .open_questions()
                    .iter()
                    .filter(|question| question.salience >= MIN_QUESTION_SALIENCE)
                    .filter(|question| {
                        has_available_agenda_reference(
                            input,
                            AgendaItemKind::OpenQuestion,
                            &format!("open_question:{}", question.id),
                        )
                    })
                    .max_by(|left, right| left.salience.total_cmp(&right.salience))
                {
                    projection.disposition = DecisionDisposition::AskQuestion;
                    projection.reference = Some(MindDecisionReference::OpenQuestion(question.id));
                    projection.push_reason(MindReasonTag::CuriosityTriggered);
                    return projection;
                }

                if let Some(item) = input
                    .mind
                    .agenda()
                    .iter()
                    .filter(|item| {
                        agenda_score(item.salience, item.activation) >= MIN_AGENDA_RESUME_SCORE
                    })
                    .filter(|item| {
                        !matches!(
                            item.kind,
                            AgendaItemKind::Interest
                                | AgendaItemKind::OpenQuestion
                                | AgendaItemKind::Curiosity
                        )
                    })
                    .max_by(|left, right| {
                        agenda_score(left.salience, left.activation)
                            .total_cmp(&agenda_score(right.salience, right.activation))
                    })
                {
                    projection.disposition = DecisionDisposition::ResumeAgenda;
                    projection.reference = Some(MindDecisionReference::Agenda(item.id));
                    projection.push_reason(MindReasonTag::AgendaResume);
                    if item.kind == AgendaItemKind::OpenLoop {
                        projection.push_reason(MindReasonTag::RelatedOpenLoop);
                    }
                    return projection;
                }

                if let Some(interest) = input
                    .mind
                    .interests()
                    .iter()
                    .filter(|interest| {
                        interest.activation >= MIN_INTEREST_ACTIVATION
                            && interest.long_term_affinity >= MIN_INTEREST_AFFINITY
                    })
                    .filter(|interest| {
                        has_available_agenda_reference(
                            input,
                            AgendaItemKind::Interest,
                            &format!("interest:{}", interest.id),
                        )
                    })
                    .max_by(|left, right| left.activation.total_cmp(&right.activation))
                {
                    projection.disposition = DecisionDisposition::ChangeTopic;
                    projection.reference = Some(MindDecisionReference::Interest(interest.id));
                    projection.push_reason(MindReasonTag::ActiveInterest);
                }
            }
            WorldEventKind::ProspectiveMemoryDue(due) => {
                let key = format!("open_loop:{}", due.open_loop_id);
                let agenda =
                    input.mind.agenda().iter().find(|item| {
                        item.kind == AgendaItemKind::OpenLoop && item.summary_key == key
                    });
                match agenda {
                    Some(item)
                        if agenda_score(item.salience, item.activation) >= MIN_DUE_AGENDA_SCORE =>
                    {
                        projection.disposition = DecisionDisposition::ResumeAgenda;
                        projection.reference = Some(MindDecisionReference::Agenda(item.id));
                        projection.push_reason(MindReasonTag::RelatedOpenLoop);
                        projection.push_reason(MindReasonTag::AgendaResume);
                    }
                    Some(_) => {
                        projection.disposition = DecisionDisposition::Defer;
                        projection.push_reason(MindReasonTag::LowSocialValue);
                    }
                    None => {}
                }
            }
            _ => {}
        }
        projection
    }

    #[must_use]
    pub const fn disposition(&self) -> DecisionDisposition {
        self.disposition
    }

    #[must_use]
    pub const fn baseline(&self) -> DecisionDisposition {
        self.baseline
    }

    #[must_use]
    pub fn reason_tags(&self) -> &[MindReasonTag] {
        &self.reason_tags
    }

    #[must_use]
    pub const fn reference(&self) -> Option<MindDecisionReference> {
        self.reference
    }

    #[must_use]
    pub const fn would_disagree(&self) -> bool {
        self.would_disagree
    }

    /// Propositions of the high-confidence, stable beliefs the current inbound
    /// message explicitly opposes (bounded). Empty unless `would_disagree`.
    #[must_use]
    pub fn belief_conflicts(&self) -> &[String] {
        &self.belief_conflicts
    }

    #[must_use]
    pub fn changes_baseline(&self) -> bool {
        self.disposition != self.baseline
    }

    #[must_use]
    pub fn reference_is_present(&self, input: &PlannerInput) -> bool {
        match self.reference {
            None => true,
            Some(MindDecisionReference::Agenda(id)) => {
                input.mind.agenda().iter().any(|item| item.id == id)
            }
            Some(MindDecisionReference::OpenQuestion(id)) => input
                .mind
                .open_questions()
                .iter()
                .any(|question| question.id == id),
            Some(MindDecisionReference::Interest(id)) => input
                .mind
                .interests()
                .iter()
                .any(|interest| interest.id == id),
        }
    }

    fn push_reason(&mut self, reason: MindReasonTag) {
        if !self.reason_tags.contains(&reason) {
            self.reason_tags.push(reason);
        }
    }
}

fn agenda_score(salience: f32, activation: f32) -> f32 {
    salience * 0.55 + activation * 0.45
}

fn has_available_agenda_reference(
    input: &PlannerInput,
    kind: AgendaItemKind,
    summary_key: &str,
) -> bool {
    input
        .mind
        .agenda()
        .iter()
        .any(|item| item.kind == kind && item.summary_key == summary_key)
}
