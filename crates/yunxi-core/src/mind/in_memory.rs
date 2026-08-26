use super::{
    AgendaItem, AgendaItemId, AgendaStatus, AgendaStore, Belief, BeliefId, BeliefStore,
    ConsolidationPlan, ConsolidationResult, CuriosityId, CuriosityItem, CuriosityStatus,
    CuriosityStore, Episode, EpisodeId, EpisodeStore, Interest, InterestId, InterestStore,
    MindConsolidationStore, MindDataErasure, MindDataErasureError, MindDataErasureFuture,
    MindScope, MindStoreError, MindStoreFuture, OpenQuestion, OpenQuestionId, OpenQuestionStatus,
    OpenQuestionStore, Preference, PreferenceId, PreferenceStore, SelfModel, SelfModelStore,
};
use crate::{ConversationId, PersonId};
use chrono::{DateTime, Utc};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::RwLock;

const MAX_IN_MEMORY_RECORDS_PER_KIND: usize = 4_096;
const MAX_STORE_QUERY_LIMIT: usize = 128;

#[derive(Debug, Clone, Default)]
struct State {
    self_model: Option<SelfModel>,
    beliefs: HashMap<BeliefId, Belief>,
    preferences: HashMap<PreferenceId, Preference>,
    interests: HashMap<InterestId, Interest>,
    curiosities: HashMap<CuriosityId, CuriosityItem>,
    open_questions: HashMap<OpenQuestionId, OpenQuestion>,
    agenda: HashMap<AgendaItemId, AgendaItem>,
    episodes: HashMap<EpisodeId, Episode>,
    version: u64,
}

#[derive(Debug, Default)]
pub struct InMemoryMindStore {
    state: RwLock<State>,
}

impl InMemoryMindStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, State> {
        self.state.read().unwrap_or_else(|lock| lock.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, State> {
        self.state.write().unwrap_or_else(|lock| lock.into_inner())
    }

    fn validate_limit(limit: usize) -> Result<(), MindStoreError> {
        if limit > MAX_STORE_QUERY_LIMIT {
            return Err(MindStoreError::InvalidRequest {
                reason: "mind query limit exceeds 128",
            });
        }
        Ok(())
    }

    fn apply_plan_to_state(
        state: &mut State,
        plan: &ConsolidationPlan,
    ) -> Result<usize, MindStoreError> {
        let mut applied = 0;
        for upsert in &plan.beliefs {
            put_versioned(
                &mut state.beliefs,
                upsert.value.id(),
                &upsert.value,
                upsert.expected_version,
                "belief",
                Belief::version,
            )?;
            applied += 1;
        }
        for upsert in &plan.preferences {
            put_versioned(
                &mut state.preferences,
                upsert.value.id(),
                &upsert.value,
                upsert.expected_version,
                "preference",
                Preference::version,
            )?;
            applied += 1;
        }
        for upsert in &plan.interests {
            put_versioned(
                &mut state.interests,
                upsert.value.id(),
                &upsert.value,
                upsert.expected_version,
                "interest",
                Interest::version,
            )?;
            applied += 1;
        }
        for upsert in &plan.open_questions {
            put_versioned(
                &mut state.open_questions,
                upsert.value.id(),
                &upsert.value,
                upsert.expected_version,
                "open_question",
                OpenQuestion::version,
            )?;
            applied += 1;
        }
        for upsert in &plan.agenda {
            put_versioned(
                &mut state.agenda,
                upsert.value.id(),
                &upsert.value,
                upsert.expected_version,
                "agenda",
                AgendaItem::version,
            )?;
            applied += 1;
        }
        for episode in &plan.episodes {
            if state.episodes.len() >= MAX_IN_MEMORY_RECORDS_PER_KIND
                && !state.episodes.contains_key(&episode.id())
            {
                return Err(MindStoreError::InvalidRequest {
                    reason: "in-memory episode capacity is full",
                });
            }
            if let Some(existing) = state.episodes.get(&episode.id()) {
                if existing != episode {
                    return Err(MindStoreError::VersionConflict {
                        kind: "episode",
                        id: episode.id().to_string(),
                        expected: 0,
                        actual: existing.version(),
                    });
                }
            } else {
                state.episodes.insert(episode.id(), episode.clone());
                applied += 1;
            }
        }
        Ok(applied)
    }
}

impl SelfModelStore for InMemoryMindStore {
    fn get(&self) -> MindStoreFuture<'_, Option<SelfModel>> {
        Box::pin(async move { Ok(self.read().self_model.clone()) })
    }

    fn put<'a>(
        &'a self,
        model: &'a SelfModel,
        expected_version: Option<u64>,
    ) -> MindStoreFuture<'a, SelfModel> {
        Box::pin(async move {
            model.validate()?;
            let mut state = self.write();
            match (&state.self_model, expected_version) {
                (None, None) if model.version() == 1 => {}
                (Some(existing), Some(expected))
                    if existing.version() == expected && model.version() == expected + 1 => {}
                (Some(existing), _) => {
                    return Err(MindStoreError::VersionConflict {
                        kind: "self_model",
                        id: "singleton".to_owned(),
                        expected: expected_version.unwrap_or(0),
                        actual: existing.version(),
                    });
                }
                (None, Some(expected)) => {
                    return Err(MindStoreError::VersionConflict {
                        kind: "self_model",
                        id: "singleton".to_owned(),
                        expected,
                        actual: 0,
                    });
                }
                (None, None) => {
                    return Err(MindStoreError::InvalidRequest {
                        reason: "new self model must start at version 1",
                    });
                }
            }
            state.self_model = Some(model.clone());
            state.version = state.version.saturating_add(1);
            Ok(model.clone())
        })
    }
}

impl BeliefStore for InMemoryMindStore {
    fn get(&self, id: BeliefId) -> MindStoreFuture<'_, Option<Belief>> {
        Box::pin(async move { Ok(self.read().beliefs.get(&id).cloned()) })
    }

    fn find_by_key<'a>(
        &'a self,
        scope: MindScope,
        proposition_key: &'a str,
    ) -> MindStoreFuture<'a, Option<Belief>> {
        Box::pin(async move {
            Ok(self
                .read()
                .beliefs
                .values()
                .find(|belief| {
                    belief.scope() == scope && belief.proposition_key() == proposition_key
                })
                .cloned())
        })
    }

    fn put<'a>(
        &'a self,
        belief: &'a Belief,
        expected_version: Option<u64>,
    ) -> MindStoreFuture<'a, Belief> {
        Box::pin(async move {
            belief.validate()?;
            let mut state = self.write();
            enforce_capacity(
                state.beliefs.len(),
                state.beliefs.contains_key(&belief.id()),
            )?;
            put_versioned(
                &mut state.beliefs,
                belief.id(),
                belief,
                expected_version,
                "belief",
                Belief::version,
            )?;
            state.version = state.version.saturating_add(1);
            Ok(belief.clone())
        })
    }

    fn relevant<'a>(
        &'a self,
        scopes: &'a [MindScope],
        query: &'a str,
        now: DateTime<Utc>,
        limit: usize,
    ) -> MindStoreFuture<'a, Vec<Belief>> {
        Box::pin(async move {
            Self::validate_limit(limit)?;
            let mut values = self
                .read()
                .beliefs
                .values()
                .filter(|belief| scopes.contains(&belief.scope()) && belief.is_active_at(now))
                .filter(|belief| {
                    query.trim().is_empty()
                        || super::relevance::lexical_relevance(belief.proposition(), query) > 0.0
                })
                .cloned()
                .collect::<Vec<_>>();
            values.sort_by(|left, right| {
                score_cmp(
                    belief_score(right, query, now),
                    belief_score(left, query, now),
                )
            });
            values.truncate(limit);
            Ok(values)
        })
    }
}

impl PreferenceStore for InMemoryMindStore {
    fn get(&self, id: PreferenceId) -> MindStoreFuture<'_, Option<Preference>> {
        Box::pin(async move { Ok(self.read().preferences.get(&id).cloned()) })
    }

    fn find_by_key<'a>(&'a self, subject_key: &'a str) -> MindStoreFuture<'a, Option<Preference>> {
        Box::pin(async move {
            Ok(self
                .read()
                .preferences
                .values()
                .find(|preference| preference.subject_key() == subject_key)
                .cloned())
        })
    }

    fn put<'a>(
        &'a self,
        preference: &'a Preference,
        expected_version: Option<u64>,
    ) -> MindStoreFuture<'a, Preference> {
        Box::pin(async move {
            preference.validate()?;
            let mut state = self.write();
            enforce_capacity(
                state.preferences.len(),
                state.preferences.contains_key(&preference.id()),
            )?;
            put_versioned(
                &mut state.preferences,
                preference.id(),
                preference,
                expected_version,
                "preference",
                Preference::version,
            )?;
            state.version = state.version.saturating_add(1);
            Ok(preference.clone())
        })
    }

    fn relevant<'a>(
        &'a self,
        query: &'a str,
        limit: usize,
    ) -> MindStoreFuture<'a, Vec<Preference>> {
        Box::pin(async move {
            Self::validate_limit(limit)?;
            let mut values = self
                .read()
                .preferences
                .values()
                .filter(|preference| {
                    query.trim().is_empty()
                        || super::relevance::lexical_relevance(preference.subject(), query) > 0.0
                })
                .cloned()
                .collect::<Vec<_>>();
            values.sort_by(|left, right| {
                score_cmp(
                    preference_score(right, query),
                    preference_score(left, query),
                )
            });
            values.truncate(limit);
            Ok(values)
        })
    }
}

impl InterestStore for InMemoryMindStore {
    fn get(&self, id: InterestId) -> MindStoreFuture<'_, Option<Interest>> {
        Box::pin(async move { Ok(self.read().interests.get(&id).cloned()) })
    }

    fn find_by_key<'a>(&'a self, topic_key: &'a str) -> MindStoreFuture<'a, Option<Interest>> {
        Box::pin(async move {
            Ok(self
                .read()
                .interests
                .values()
                .find(|interest| interest.topic_key() == topic_key)
                .cloned())
        })
    }

    fn put<'a>(
        &'a self,
        interest: &'a Interest,
        expected_version: Option<u64>,
    ) -> MindStoreFuture<'a, Interest> {
        Box::pin(async move {
            interest.validate()?;
            let mut state = self.write();
            enforce_capacity(
                state.interests.len(),
                state.interests.contains_key(&interest.id()),
            )?;
            put_versioned(
                &mut state.interests,
                interest.id(),
                interest,
                expected_version,
                "interest",
                Interest::version,
            )?;
            state.version = state.version.saturating_add(1);
            Ok(interest.clone())
        })
    }

    fn relevant<'a>(&'a self, query: &'a str, limit: usize) -> MindStoreFuture<'a, Vec<Interest>> {
        Box::pin(async move {
            Self::validate_limit(limit)?;
            let mut values = self
                .read()
                .interests
                .values()
                .filter(|interest| {
                    query.trim().is_empty()
                        || super::relevance::lexical_relevance(interest.topic(), query) > 0.0
                })
                .cloned()
                .collect::<Vec<_>>();
            values.sort_by(|left, right| {
                score_cmp(interest_score(right, query), interest_score(left, query))
            });
            values.truncate(limit);
            Ok(values)
        })
    }
}

impl CuriosityStore for InMemoryMindStore {
    fn get(&self, id: CuriosityId) -> MindStoreFuture<'_, Option<CuriosityItem>> {
        Box::pin(async move { Ok(self.read().curiosities.get(&id).cloned()) })
    }

    fn find_open_by_key<'a>(
        &'a self,
        scope: MindScope,
        question_key: &'a str,
    ) -> MindStoreFuture<'a, Option<CuriosityItem>> {
        Box::pin(async move {
            Ok(self
                .read()
                .curiosities
                .values()
                .find(|item| {
                    item.scope() == scope
                        && item.question_key() == question_key
                        && !item.status().is_terminal()
                })
                .cloned())
        })
    }

    fn put<'a>(
        &'a self,
        curiosity: &'a CuriosityItem,
        expected_version: Option<u64>,
    ) -> MindStoreFuture<'a, CuriosityItem> {
        Box::pin(async move {
            curiosity.validate()?;
            let mut state = self.write();
            enforce_capacity(
                state.curiosities.len(),
                state.curiosities.contains_key(&curiosity.id()),
            )?;
            put_versioned(
                &mut state.curiosities,
                curiosity.id(),
                curiosity,
                expected_version,
                "curiosity",
                CuriosityItem::version,
            )?;
            state.version = state.version.saturating_add(1);
            Ok(curiosity.clone())
        })
    }

    fn list_open<'a>(
        &'a self,
        scopes: &'a [MindScope],
        now: DateTime<Utc>,
        limit: usize,
    ) -> MindStoreFuture<'a, Vec<CuriosityItem>> {
        Box::pin(async move {
            Self::validate_limit(limit)?;
            let mut values = self
                .read()
                .curiosities
                .values()
                .filter(|item| {
                    scopes.contains(&item.scope())
                        && !item.status().is_terminal()
                        && item.expires_at().is_none_or(|expires| expires > now)
                })
                .cloned()
                .collect::<Vec<_>>();
            values.sort_by(|left, right| {
                score_cmp(right.salience(), left.salience())
                    .then_with(|| right.updated_at().cmp(&left.updated_at()))
            });
            values.truncate(limit);
            Ok(values)
        })
    }
}

impl OpenQuestionStore for InMemoryMindStore {
    fn get(&self, id: OpenQuestionId) -> MindStoreFuture<'_, Option<OpenQuestion>> {
        Box::pin(async move { Ok(self.read().open_questions.get(&id).cloned()) })
    }

    fn find_open_by_key<'a>(
        &'a self,
        scope: MindScope,
        question_key: &'a str,
    ) -> MindStoreFuture<'a, Option<OpenQuestion>> {
        Box::pin(async move {
            Ok(self
                .read()
                .open_questions
                .values()
                .find(|item| {
                    item.scope() == scope
                        && item.question_key() == question_key
                        && item.status() == OpenQuestionStatus::Open
                })
                .cloned())
        })
    }

    fn put<'a>(
        &'a self,
        question: &'a OpenQuestion,
        expected_version: Option<u64>,
    ) -> MindStoreFuture<'a, OpenQuestion> {
        Box::pin(async move {
            question.validate()?;
            let mut state = self.write();
            enforce_capacity(
                state.open_questions.len(),
                state.open_questions.contains_key(&question.id()),
            )?;
            put_versioned(
                &mut state.open_questions,
                question.id(),
                question,
                expected_version,
                "open_question",
                OpenQuestion::version,
            )?;
            state.version = state.version.saturating_add(1);
            Ok(question.clone())
        })
    }

    fn list_open<'a>(
        &'a self,
        scopes: &'a [MindScope],
        limit: usize,
    ) -> MindStoreFuture<'a, Vec<OpenQuestion>> {
        Box::pin(async move {
            Self::validate_limit(limit)?;
            let mut values = self
                .read()
                .open_questions
                .values()
                .filter(|item| {
                    scopes.contains(&item.scope()) && item.status() == OpenQuestionStatus::Open
                })
                .cloned()
                .collect::<Vec<_>>();
            values.sort_by(|left, right| {
                score_cmp(right.salience(), left.salience())
                    .then_with(|| right.updated_at().cmp(&left.updated_at()))
            });
            values.truncate(limit);
            Ok(values)
        })
    }
}

impl AgendaStore for InMemoryMindStore {
    fn get(&self, id: AgendaItemId) -> MindStoreFuture<'_, Option<AgendaItem>> {
        Box::pin(async move { Ok(self.read().agenda.get(&id).cloned()) })
    }

    fn find_active_by_key<'a>(
        &'a self,
        scope: MindScope,
        subject_key: &'a str,
    ) -> MindStoreFuture<'a, Option<AgendaItem>> {
        Box::pin(async move {
            Ok(self
                .read()
                .agenda
                .values()
                .find(|item| {
                    item.scope() == scope
                        && item.subject().dedupe_key() == subject_key
                        && !item.status().is_terminal()
                })
                .cloned())
        })
    }

    fn put<'a>(
        &'a self,
        item: &'a AgendaItem,
        expected_version: Option<u64>,
    ) -> MindStoreFuture<'a, AgendaItem> {
        Box::pin(async move {
            item.validate()?;
            let mut state = self.write();
            enforce_capacity(state.agenda.len(), state.agenda.contains_key(&item.id()))?;
            put_versioned(
                &mut state.agenda,
                item.id(),
                item,
                expected_version,
                "agenda",
                AgendaItem::version,
            )?;
            state.version = state.version.saturating_add(1);
            Ok(item.clone())
        })
    }

    fn list_active<'a>(
        &'a self,
        scopes: &'a [MindScope],
        now: DateTime<Utc>,
        limit: usize,
    ) -> MindStoreFuture<'a, Vec<AgendaItem>> {
        Box::pin(async move {
            Self::validate_limit(limit)?;
            let mut values = self
                .read()
                .agenda
                .values()
                .filter(|item| scopes.contains(&item.scope()) && item.is_available_at(now))
                .cloned()
                .collect::<Vec<_>>();
            values.sort_by(|left, right| score_cmp(right.rank_score(), left.rank_score()));
            values.truncate(limit);
            Ok(values)
        })
    }
}

impl EpisodeStore for InMemoryMindStore {
    fn put<'a>(&'a self, episode: &'a Episode) -> MindStoreFuture<'a, Episode> {
        Box::pin(async move {
            episode.validate()?;
            let mut state = self.write();
            enforce_capacity(
                state.episodes.len(),
                state.episodes.contains_key(&episode.id()),
            )?;
            if let Some(existing) = state.episodes.get(&episode.id()) {
                if existing == episode {
                    return Ok(existing.clone());
                }
                return Err(MindStoreError::VersionConflict {
                    kind: "episode",
                    id: episode.id().to_string(),
                    expected: 0,
                    actual: existing.version(),
                });
            }
            state.episodes.insert(episode.id(), episode.clone());
            state.version = state.version.saturating_add(1);
            Ok(episode.clone())
        })
    }

    fn list_recent<'a>(
        &'a self,
        scopes: &'a [MindScope],
        since: DateTime<Utc>,
        limit: usize,
    ) -> MindStoreFuture<'a, Vec<Episode>> {
        Box::pin(async move {
            Self::validate_limit(limit)?;
            let mut values = self
                .read()
                .episodes
                .values()
                .filter(|episode| {
                    scopes.contains(&episode.scope()) && episode.occurred_at() >= since
                })
                .cloned()
                .collect::<Vec<_>>();
            values.sort_by_key(|episode| std::cmp::Reverse(episode.occurred_at()));
            values.truncate(limit);
            Ok(values)
        })
    }
}

impl MindConsolidationStore for InMemoryMindStore {
    fn apply<'a>(
        &'a self,
        plan: &'a ConsolidationPlan,
    ) -> MindStoreFuture<'a, ConsolidationResult> {
        Box::pin(async move {
            plan.validate(super::ConsolidationConfig {
                max_belief_delta: 1.0,
                max_preference_delta: 1.0,
                max_interest_affinity_delta: 1.0,
                max_updates_per_reflection: 128,
            })?;
            let mut state = self.write();
            if state.version != plan.base_mind_version {
                return Err(MindStoreError::VersionConflict {
                    kind: "mind",
                    id: "global".to_owned(),
                    expected: plan.base_mind_version,
                    actual: state.version,
                });
            }
            let mut next = state.clone();
            let applied_updates = Self::apply_plan_to_state(&mut next, plan)?;
            next.version = next.version.saturating_add(1);
            let new_mind_version = next.version;
            *state = next;
            Ok(ConsolidationResult {
                applied_updates,
                new_mind_version,
            })
        })
    }

    fn current_version(&self) -> MindStoreFuture<'_, u64> {
        Box::pin(async move { Ok(self.read().version) })
    }
}

impl MindDataErasure for InMemoryMindStore {
    fn erase_person(&self, person_id: PersonId) -> MindDataErasureFuture<'_> {
        Box::pin(async move {
            let mut state = self.write();
            state
                .beliefs
                .retain(|_, item| item.scope().person_id() != Some(person_id));
            state.curiosities.retain(|_, item| {
                item.subject() != Some(person_id) && item.scope().person_id() != Some(person_id)
            });
            state
                .open_questions
                .retain(|_, item| item.scope().person_id() != Some(person_id));
            state
                .agenda
                .retain(|_, item| item.scope().person_id() != Some(person_id));
            state.episodes.retain(|_, item| {
                item.scope().person_id() != Some(person_id)
                    && !item.participants().contains(&person_id)
            });
            state.version = state.version.saturating_add(1);
            Ok(())
        })
    }

    fn erase_conversation(&self, conversation_id: ConversationId) -> MindDataErasureFuture<'_> {
        Box::pin(async move {
            let mut state = self.write();
            state
                .beliefs
                .retain(|_, item| item.scope().conversation_id() != Some(conversation_id));
            state.curiosities.retain(|_, item| {
                item.conversation_id() != Some(conversation_id)
                    && item.scope().conversation_id() != Some(conversation_id)
            });
            state
                .open_questions
                .retain(|_, item| item.scope().conversation_id() != Some(conversation_id));
            state
                .agenda
                .retain(|_, item| item.scope().conversation_id() != Some(conversation_id));
            state
                .episodes
                .retain(|_, item| item.scope().conversation_id() != Some(conversation_id));
            state.version = state.version.saturating_add(1);
            Ok(())
        })
    }
}

fn put_versioned<K, V>(
    records: &mut HashMap<K, V>,
    id: K,
    value: &V,
    expected_version: Option<u64>,
    kind: &'static str,
    version: impl Fn(&V) -> u64,
) -> Result<(), MindStoreError>
where
    K: Eq + std::hash::Hash + Copy + ToString,
    V: Clone,
{
    match (records.get(&id), expected_version) {
        (None, None) if version(value) == 1 => {
            records.insert(id, value.clone());
            Ok(())
        }
        (Some(existing), Some(expected))
            if version(existing) == expected && version(value) == expected.saturating_add(1) =>
        {
            records.insert(id, value.clone());
            Ok(())
        }
        (Some(existing), _) => Err(MindStoreError::VersionConflict {
            kind,
            id: id.to_string(),
            expected: expected_version.unwrap_or(0),
            actual: version(existing),
        }),
        (None, Some(expected)) => Err(MindStoreError::VersionConflict {
            kind,
            id: id.to_string(),
            expected,
            actual: 0,
        }),
        (None, None) => Err(MindStoreError::InvalidRequest {
            reason: "new mind records must start at version 1",
        }),
    }
}

fn enforce_capacity(length: usize, already_exists: bool) -> Result<(), MindStoreError> {
    if !already_exists && length >= MAX_IN_MEMORY_RECORDS_PER_KIND {
        Err(MindStoreError::InvalidRequest {
            reason: "in-memory mind store capacity is full",
        })
    } else {
        Ok(())
    }
}

fn lexical_score(value: &str, query: &str) -> f32 {
    super::relevance::lexical_relevance(value, query)
}

fn recency_score(updated_at: DateTime<Utc>, now: DateTime<Utc>) -> f32 {
    let age_hours = now.signed_duration_since(updated_at).num_minutes().max(0) as f32 / 60.0;
    1.0 / (1.0 + age_hours / (24.0 * 30.0))
}

fn belief_score(belief: &Belief, query: &str, now: DateTime<Utc>) -> f32 {
    lexical_score(belief.proposition(), query) * 0.45
        + belief.confidence() * 0.25
        + belief.stability() * 0.15
        + recency_score(belief.updated_at(), now) * 0.15
}

fn preference_score(preference: &Preference, query: &str) -> f32 {
    lexical_score(preference.subject(), query) * 0.55
        + preference.intensity() * preference.confidence() * 0.45
}

fn interest_score(interest: &Interest, query: &str) -> f32 {
    lexical_score(interest.topic(), query) * 0.45
        + interest.activation() * 0.3
        + interest.long_term_affinity() * 0.2
        + interest.novelty() * 0.05
}

fn score_cmp(left: f32, right: f32) -> Ordering {
    left.partial_cmp(&right).unwrap_or(Ordering::Equal)
}

#[allow(dead_code)]
fn _error_type_is_send_sync(_: MindDataErasureError) {}

#[allow(dead_code)]
fn _statuses_are_distinct(_: CuriosityStatus, _: AgendaStatus) {}
