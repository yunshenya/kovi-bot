use super::*;
use crate::{
    ConversationId, ConversationKind, DecisionDisposition, EventPriority, EventScope,
    MessageContent, MessageId, MessageReceivedEvent, PersonId, PlannerInput, PlannerStateSnapshot,
    ProspectiveMemoryEvent, WorldEvent, WorldEventKind,
};
use chrono::{Duration, TimeZone, Utc};
use std::sync::Arc;

fn now() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 26, 12, 0, 0)
        .single()
        .expect("fixed test timestamp")
}

fn direct_event(person_id: PersonId, conversation_id: ConversationId, text: &str) -> WorldEvent {
    WorldEvent::message_received(
        EventPriority::Normal,
        MessageReceivedEvent {
            message_id: MessageId::new(),
            conversation_id,
            sender: person_id,
            content: MessageContent::text(text),
            reply_to: None,
            timestamp: now(),
            conversation_kind: ConversationKind::Direct,
            addressed_to_agent: true,
            replies_to_agent: false,
            stop_requested: false,
            explicit_request: true,
            visible_reply_allowed: true,
        },
    )
}

fn casual_direct_event(
    person_id: PersonId,
    conversation_id: ConversationId,
    text: &str,
) -> WorldEvent {
    let WorldEventKind::MessageReceived(mut message) =
        direct_event(person_id, conversation_id, text)
            .kind()
            .clone()
    else {
        unreachable!()
    };
    message.addressed_to_agent = false;
    message.explicit_request = false;
    WorldEvent::message_received(EventPriority::Normal, message)
}

fn active_snapshot(
    beliefs: &[Belief],
    preferences: &[Preference],
    interests: &[Interest],
    questions: &[OpenQuestion],
    agenda: &[AgendaItem],
) -> MindSnapshot {
    MindSnapshot::new(
        Some(SelfModelSnapshot::from_model(&SelfModel::seed_yunxi(now())).expect("self model")),
        beliefs
            .iter()
            .map(BeliefSnapshot::try_from)
            .collect::<Result<Vec<_>, _>>()
            .expect("belief snapshots"),
        preferences
            .iter()
            .map(PreferenceSnapshot::try_from)
            .collect::<Result<Vec<_>, _>>()
            .expect("preference snapshots"),
        interests
            .iter()
            .map(InterestSnapshot::try_from)
            .collect::<Result<Vec<_>, _>>()
            .expect("interest snapshots"),
        questions
            .iter()
            .map(OpenQuestionSnapshot::try_from)
            .collect::<Result<Vec<_>, _>>()
            .expect("question snapshots"),
        agenda
            .iter()
            .map(AgendaItemSnapshot::try_from)
            .collect::<Result<Vec<_>, _>>()
            .expect("agenda snapshots"),
        Vec::new(),
        MindInfluenceMode::Active,
        1,
        now(),
    )
    .expect("active snapshot")
}

fn projected(event: WorldEvent, snapshot: MindSnapshot) -> MindDecisionProjection {
    let input = PlannerInput::new(event, PlannerStateSnapshot::empty()).with_mind(snapshot);
    MindDecisionProjection::for_input(&input, DecisionDisposition::Reply)
}

#[test]
fn scenario_a_belief_conflict_shapes_but_does_not_override_explicit_reply() {
    let belief = Belief::new(
        BeliefId::new(),
        MindScope::Global,
        "Rust 的严格类型系统总体有价值",
        0.8,
        0.8,
        BeliefSource::Experience,
        Vec::new(),
        None,
        now(),
    )
    .expect("belief");
    let projection = projected(
        direct_event(
            PersonId::new(),
            ConversationId::new(),
            "Rust 就是一坨垃圾，对吧？",
        ),
        active_snapshot(&[belief], &[], &[], &[], &[]),
    );

    assert_eq!(projection.disposition(), DecisionDisposition::Reply);
    assert!(projection.would_disagree());
    assert!(
        projection
            .reason_tags()
            .contains(&MindReasonTag::BeliefConflict)
    );
}

#[test]
fn strong_belief_does_not_claim_disagreement_when_user_is_aligned_or_unrelated() {
    let belief = Belief::new(
        BeliefId::new(),
        MindScope::Global,
        "Rust 的严格类型系统总体有价值",
        0.9,
        0.8,
        BeliefSource::Experience,
        Vec::new(),
        None,
        now(),
    )
    .expect("belief");
    let snapshot = active_snapshot(&[belief], &[], &[], &[], &[]);

    let aligned = projected(
        direct_event(
            PersonId::new(),
            ConversationId::new(),
            "我也觉得 Rust 的严格类型系统很有价值。",
        ),
        snapshot.clone(),
    );
    assert!(!aligned.would_disagree());
    assert!(
        !aligned
            .reason_tags()
            .contains(&MindReasonTag::BeliefConflict)
    );

    let unrelated = projected(
        direct_event(
            PersonId::new(),
            ConversationId::new(),
            "PostgreSQL 的错误信息有时很糟糕。",
        ),
        snapshot,
    );
    assert!(!unrelated.would_disagree());
    assert!(
        !unrelated
            .reason_tags()
            .contains(&MindReasonTag::BeliefConflict)
    );
}

#[test]
fn scenario_b_reliable_counter_evidence_can_reduce_belief_confidence() {
    let belief = Belief::new(
        BeliefId::new(),
        MindScope::Global,
        "A 是当前最可靠的方案",
        0.7,
        0.6,
        BeliefSource::Experience,
        Vec::new(),
        None,
        now(),
    )
    .expect("belief");
    let counter_evidence = EvidenceRef::new(
        EvidenceKind::Event(crate::EventId::new()),
        EvidencePolarity::Contradicts,
        1.0,
        now() + Duration::minutes(1),
    )
    .expect("counter evidence");
    let revised = belief
        .apply_delta(
            -0.2,
            -0.1,
            &[counter_evidence],
            now() + Duration::minutes(1),
        )
        .expect("belief revision");

    assert!((revised.confidence() - 0.5).abs() < f32::EPSILON);
    assert!(revised.stability() < belief.stability());
    assert_eq!(revised.contradiction_count(), 1);
}

#[test]
fn scenario_c_self_model_without_belief_or_preference_does_not_invent_an_opinion() {
    let snapshot = active_snapshot(&[], &[], &[], &[], &[]);
    let projection = projected(
        direct_event(
            PersonId::new(),
            ConversationId::new(),
            "你更喜欢 A 还是 B？",
        ),
        snapshot.clone(),
    );

    assert!(snapshot.beliefs().is_empty());
    assert!(snapshot.preferences().is_empty());
    assert!(!projection.would_disagree());
    assert!(projection.reason_tags().is_empty());
    assert_eq!(projection.disposition(), DecisionDisposition::Reply);
}

#[test]
fn scenario_d_curiosity_agenda_does_not_interrupt_the_current_request() {
    let agenda = AgendaItem::new(
        AgendaItemId::new(),
        MindScope::Person {
            person_id: PersonId::new(),
        },
        AgendaSubject::Curiosity(CuriosityId::new()),
        0.95,
        0.95,
        0.5,
        AgendaSource::Curiosity,
        now(),
    )
    .expect("curiosity agenda");
    let projection = projected(
        direct_event(
            PersonId::new(),
            ConversationId::new(),
            "我最近换工作了，先帮我看下这段代码。",
        ),
        active_snapshot(&[], &[], &[], &[], &[agenda]),
    );

    assert_eq!(projection.disposition(), DecisionDisposition::Reply);
    assert_eq!(projection.reference(), None);
}

#[test]
fn scenario_e_open_loop_agenda_can_resume_during_a_natural_lull() {
    let agenda = AgendaItem::new(
        AgendaItemId::new(),
        MindScope::Conversation {
            conversation_id: ConversationId::new(),
        },
        AgendaSubject::OpenLoop(crate::OpenLoopId::new()),
        0.9,
        0.9,
        0.7,
        AgendaSource::OpenLoop,
        now(),
    )
    .expect("open-loop agenda");
    let agenda_id = agenda.id();
    let projection = projected(
        casual_direct_event(
            PersonId::new(),
            ConversationId::new(),
            "Rust 那段已经聊完了。",
        ),
        active_snapshot(&[], &[], &[], &[], &[agenda]),
    );

    assert_eq!(projection.disposition(), DecisionDisposition::ResumeAgenda);
    assert_eq!(
        projection.reference(),
        Some(MindDecisionReference::Agenda(agenda_id))
    );
    assert!(
        projection
            .reason_tags()
            .contains(&MindReasonTag::RelatedOpenLoop)
    );
}

#[test]
fn scenario_f_group_discussion_stays_silent_despite_an_active_interest() {
    let interest = Interest::new(
        InterestId::new(),
        "Rust 类型系统",
        0.95,
        0.9,
        0.8,
        MindSource::Experience,
        now(),
    )
    .expect("interest");
    let WorldEventKind::MessageReceived(mut message) = casual_direct_event(
        PersonId::new(),
        ConversationId::new(),
        "这个 Rust 类型设计挺有意思。",
    )
    .kind()
    .clone() else {
        unreachable!()
    };
    message.conversation_kind = ConversationKind::Group;
    let projection = projected(
        WorldEvent::message_received(EventPriority::Normal, message),
        active_snapshot(&[], &[], &[interest], &[], &[]),
    );

    assert_eq!(projection.disposition(), DecisionDisposition::Silent);
    assert_eq!(projection.reference(), None);
    assert!(
        projection
            .reason_tags()
            .contains(&MindReasonTag::LowSocialValue)
    );
}

#[test]
fn scenario_g_open_question_waits_for_a_natural_follow_up_turn() {
    let question = OpenQuestion::new(
        OpenQuestionId::new(),
        MindScope::Global,
        "前后说法是否指不同频率？",
        Vec::new(),
        0.8,
        now(),
    )
    .expect("open question");
    let question_id = question.id();
    let agenda = AgendaItem::new(
        AgendaItemId::new(),
        MindScope::Global,
        AgendaSubject::OpenQuestion(question_id),
        0.8,
        0.8,
        0.5,
        AgendaSource::OpenQuestion,
        now(),
    )
    .expect("open-question agenda");
    let snapshot = active_snapshot(&[], &[], &[], std::slice::from_ref(&question), &[agenda]);
    let immediate = projected(
        direct_event(
            PersonId::new(),
            ConversationId::new(),
            "先回答我当前这个问题。",
        ),
        snapshot.clone(),
    );
    let later = projected(
        casual_direct_event(
            PersonId::new(),
            ConversationId::new(),
            "刚才那个问题说完了。",
        ),
        snapshot,
    );

    assert_eq!(immediate.disposition(), DecisionDisposition::Reply);
    assert_eq!(later.disposition(), DecisionDisposition::AskQuestion);
    assert_eq!(
        later.reference(),
        Some(MindDecisionReference::OpenQuestion(question_id))
    );
    let cooling_down = projected(
        casual_direct_event(PersonId::new(), ConversationId::new(), "再聊一会儿。"),
        active_snapshot(&[], &[], &[], &[question], &[]),
    );
    assert_eq!(cooling_down.disposition(), DecisionDisposition::Reply);
}

#[test]
fn scenario_h_day_boundary_reflects_over_multiple_events_as_one_batch() {
    let event = direct_event(PersonId::new(), ConversationId::new(), "今天的第一件事");
    let recent_events = (0..3)
        .map(|offset| ReflectionEvent {
            event_id: crate::EventId::new(),
            scope: MindScope::Global,
            summary: format!("同一主题事件 {offset}"),
            salience: 0.4,
            occurred_at: now() - Duration::minutes(i64::from(offset)),
        })
        .collect();
    let input = ReflectionInput {
        trigger: ReflectionTrigger::DayBoundary,
        depth: ReflectionDepth::Deep,
        scope: MindScope::Global,
        recent_events,
        salient_memories: vec!["当天相关记忆".to_owned()],
        open_loop_summaries: Vec::new(),
        goal_summaries: vec!["完成同一主题目标".to_owned()],
        mind: MindSnapshot::empty(),
        requested_at: now(),
        trace: event.trace(),
    };

    assert!(input.validate().is_ok());
    assert!(input.should_reflect());
    assert_eq!(input.recent_events.len(), 3);
}

#[tokio::test]
async fn scenario_i_single_preference_proposal_is_clamped_by_consolidation() {
    let store = Arc::new(InMemoryMindStore::new());
    let services = MindServices::from_store(Arc::clone(&store));
    let base = services
        .consolidation
        .current_version()
        .await
        .expect("mind version");
    let event = direct_event(PersonId::new(), ConversationId::new(), "一次正向经历");
    let input = ReflectionInput {
        trigger: ReflectionTrigger::HighSalienceEvent,
        depth: ReflectionDepth::Deep,
        scope: MindScope::Global,
        recent_events: Vec::new(),
        salient_memories: Vec::new(),
        open_loop_summaries: Vec::new(),
        goal_summaries: Vec::new(),
        mind: MindSnapshot::new(
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            MindInfluenceMode::Shadow,
            base,
            now(),
        )
        .expect("versioned snapshot"),
        requested_at: now(),
        trace: event.trace(),
    };
    let mut proposal = ReflectionProposal::empty(&input);
    proposal.preference_updates.push(PreferenceUpdateProposal {
        operation: PreferenceOperation::Upsert,
        preference_id: None,
        expected_version: None,
        subject: "严格类型系统".to_owned(),
        valence_delta: 1.0,
        intensity_delta: 1.0,
        confidence_delta: 1.0,
        source: PreferenceSource::Experience,
    });

    let plan = Consolidation::new(ConsolidationConfig::default())
        .expect("consolidation")
        .prepare(&services, &proposal)
        .await
        .expect("bounded plan");
    let preference = &plan.preferences[0].value;
    assert!(preference.valence() <= 0.1);
    assert!(preference.intensity() <= 0.1);
    assert!(preference.confidence() < 1.0);
}

#[test]
fn scenario_j_self_identity_is_identical_across_host_contexts() {
    let kovi_host = active_snapshot(&[], &[], &[], &[], &[]);
    let cli_host = active_snapshot(&[], &[], &[], &[], &[]);
    let kovi_identity = kovi_host.self_model().expect("Kovi self model").identity();
    let cli_identity = cli_host.self_model().expect("CLI self model").identity();

    assert_eq!(kovi_identity, cli_identity);
    assert_eq!(kovi_identity.name(), "芸汐");
    assert!(kovi_identity.is_host_independent());
}

#[test]
fn active_interest_can_change_topic_in_a_direct_conversation_lull() {
    let interest = Interest::new(
        InterestId::new(),
        "AI Agent",
        0.9,
        0.8,
        0.7,
        MindSource::Experience,
        now(),
    )
    .expect("interest");
    let interest_id = interest.id();
    let agenda = AgendaItem::new(
        AgendaItemId::new(),
        MindScope::Global,
        AgendaSubject::Interest(interest_id),
        0.9,
        0.9,
        0.5,
        AgendaSource::Interest,
        now(),
    )
    .expect("interest agenda");
    let projection = projected(
        casual_direct_event(PersonId::new(), ConversationId::new(), "这个话题先到这里。"),
        active_snapshot(&[], &[], std::slice::from_ref(&interest), &[], &[agenda]),
    );

    assert_eq!(projection.disposition(), DecisionDisposition::ChangeTopic);
    assert_eq!(
        projection.reference(),
        Some(MindDecisionReference::Interest(interest_id))
    );
    let cooling_down = projected(
        casual_direct_event(PersonId::new(), ConversationId::new(), "继续吧。"),
        active_snapshot(&[], &[], &[interest], &[], &[]),
    );
    assert_eq!(cooling_down.disposition(), DecisionDisposition::Reply);
}

#[test]
fn low_value_due_open_loop_is_deferred_instead_of_forcing_a_message() {
    let open_loop_id = crate::OpenLoopId::new();
    let agenda = AgendaItem::new(
        AgendaItemId::new(),
        MindScope::Global,
        AgendaSubject::OpenLoop(open_loop_id),
        0.2,
        0.2,
        0.5,
        AgendaSource::OpenLoop,
        now(),
    )
    .expect("low-value agenda");
    let event = WorldEvent::new(
        now(),
        EventScope::Global,
        EventPriority::Normal,
        WorldEventKind::ProspectiveMemoryDue(ProspectiveMemoryEvent { open_loop_id }),
    );
    let projection = projected(event, active_snapshot(&[], &[], &[], &[], &[agenda]));

    assert_eq!(projection.disposition(), DecisionDisposition::Defer);
    assert!(
        projection
            .reason_tags()
            .contains(&MindReasonTag::LowSocialValue)
    );
}

#[test]
fn self_model_keeps_stable_host_independent_identity() {
    let model = SelfModel::seed_yunxi(now());
    assert_eq!(model.identity().name(), "芸汐");
    assert!(model.identity().is_ai_driven());
    assert!(model.identity().is_host_independent());
    assert_eq!(model.traits().len(), 6);
    assert!(model.validate().is_ok());
}

#[test]
fn belief_updates_clamp_and_track_contradictions() {
    let evidence = EvidenceRef::new(
        EvidenceKind::Event(crate::EventId::new()),
        EvidencePolarity::Supports,
        0.8,
        now(),
    )
    .expect("valid evidence");
    let belief = Belief::new(
        BeliefId::new(),
        MindScope::Global,
        "Rust 的严格类型系统总体有价值",
        0.9,
        0.8,
        BeliefSource::Experience,
        vec![evidence],
        None,
        now(),
    )
    .expect("valid belief");
    let contradiction = EvidenceRef::new(
        EvidenceKind::Event(crate::EventId::new()),
        EvidencePolarity::Contradicts,
        1.0,
        now() + Duration::minutes(1),
    )
    .expect("valid evidence");
    let updated = belief
        .apply_delta(-1.0, 0.5, &[contradiction], now() + Duration::minutes(1))
        .expect("bounded update");
    assert_eq!(updated.confidence(), 0.0);
    assert_eq!(updated.stability(), 1.0);
    assert_eq!(updated.contradiction_count(), 1);
}

#[test]
fn inferred_sensitive_person_belief_is_rejected() {
    let error = Belief::new(
        BeliefId::new(),
        MindScope::Person {
            person_id: PersonId::new(),
        },
        "这个人的政治倾向可能是某种立场",
        0.5,
        0.2,
        BeliefSource::Inference,
        Vec::new(),
        None,
        now(),
    )
    .expect_err("sensitive inference must fail");
    assert_eq!(error, MindValidationError::SensitivePersonInference);
}

#[test]
fn preference_and_interest_evolve_at_bounded_rates() {
    let preference = Preference::new(
        PreferenceId::new(),
        "严格类型系统",
        0.95,
        0.9,
        0.8,
        0.8,
        PreferenceSource::Experience,
        now(),
    )
    .expect("valid preference")
    .apply_delta(0.5, 0.5, 0.5, now() + Duration::minutes(1))
    .expect("bounded preference update");
    assert_eq!(preference.valence(), 1.0);
    assert_eq!(preference.intensity(), 1.0);
    assert_eq!(preference.confidence(), 1.0);

    let interest = Interest::new(
        InterestId::new(),
        "AI Agent",
        1.0,
        0.85,
        0.8,
        MindSource::Seed,
        now(),
    )
    .expect("valid interest");
    let decayed = interest
        .decay(now() + Duration::hours(6), 6.0 * 60.0 * 60.0)
        .expect("valid decay");
    assert!((decayed.activation() - 0.5).abs() < 0.001);
    assert_eq!(decayed.long_term_affinity(), 0.85);
}

#[test]
fn curiosity_expires_and_open_question_cannot_resurrect() {
    let curiosity = CuriosityItem::new(
        CuriosityId::new(),
        "为什么换工作？",
        Some(PersonId::new()),
        None,
        0.7,
        now(),
        Some(now() + Duration::hours(1)),
    )
    .expect("valid curiosity");
    let expired = curiosity
        .expire_if_due(now() + Duration::hours(2))
        .expect("expiry transition");
    assert_eq!(expired.status(), CuriosityStatus::Expired);

    let question = OpenQuestion::new(
        OpenQuestionId::new(),
        MindScope::Global,
        "前后说法是否指不同频率？",
        Vec::new(),
        0.5,
        now(),
    )
    .expect("valid open question")
    .transition(OpenQuestionStatus::Resolved, now() + Duration::minutes(1))
    .expect("resolution");
    assert!(
        question
            .transition(OpenQuestionStatus::Open, now() + Duration::minutes(2))
            .is_err()
    );
}

#[test]
fn agenda_dedupes_and_prunes_to_scope_limits() {
    let person = PersonId::new();
    let scope = MindScope::Person { person_id: person };
    let limits = InnerAgendaLimits {
        max_total: 2,
        max_per_person: 1,
        max_per_conversation: 1,
    };
    let mut agenda = InnerAgenda::empty(now());
    let low = AgendaItem::new(
        AgendaItemId::new(),
        scope,
        AgendaSubject::Curiosity(CuriosityId::new()),
        0.2,
        0.2,
        0.1,
        AgendaSource::Curiosity,
        now(),
    )
    .expect("valid item");
    let high = AgendaItem::new(
        AgendaItemId::new(),
        scope,
        AgendaSubject::Goal(crate::GoalId::new()),
        0.9,
        0.9,
        0.9,
        AgendaSource::Goal,
        now(),
    )
    .expect("valid item");
    agenda
        .upsert(low, limits, now())
        .expect("first agenda insert");
    agenda
        .upsert(high.clone(), limits, now())
        .expect("scope pruning");
    assert_eq!(agenda.items().len(), 1);
    assert_eq!(agenda.items()[0].id(), high.id());

    let refreshed = high
        .activate(1.0, now() + Duration::minutes(1))
        .expect("activation");
    agenda
        .upsert(refreshed, limits, now() + Duration::minutes(1))
        .expect("same id updates in place");
    assert_eq!(agenda.items().len(), 1);
}

#[tokio::test]
async fn snapshot_is_bounded_and_group_scope_does_not_leak_person_beliefs() {
    let store = Arc::new(InMemoryMindStore::new());
    let services = MindServices::from_store(Arc::clone(&store));
    services
        .self_model
        .put(&SelfModel::seed_yunxi(now()), None)
        .await
        .expect("seed self model");
    let person = PersonId::new();
    let conversation = ConversationId::new();
    let global = Belief::new(
        BeliefId::new(),
        MindScope::Global,
        "Rust 类型系统有价值",
        0.8,
        0.8,
        BeliefSource::Seed,
        Vec::new(),
        None,
        now(),
    )
    .expect("global belief");
    let private = Belief::new(
        BeliefId::new(),
        MindScope::Person { person_id: person },
        "用户喜欢 Rust",
        0.7,
        0.5,
        BeliefSource::Conversation,
        Vec::new(),
        None,
        now(),
    )
    .expect("private belief");
    services
        .beliefs
        .put(&global, None)
        .await
        .expect("store global belief");
    services
        .beliefs
        .put(&private, None)
        .await
        .expect("store private belief");

    let provider = MindSnapshotStoreProvider::new(services.clone());
    let direct = direct_event(person, conversation, "Rust");
    let direct_request = MindSnapshotRequest::for_event(
        &direct,
        None,
        MindSnapshotLimits {
            beliefs: 1,
            ..MindSnapshotLimits::default()
        },
        MindInfluenceMode::Shadow,
    )
    .expect("valid request");
    let snapshot = provider.snapshot(&direct_request).await.expect("snapshot");
    assert_eq!(snapshot.beliefs().len(), 1);
    assert_eq!(snapshot.influence_mode(), MindInfluenceMode::Shadow);

    let group = match direct.kind().clone() {
        crate::WorldEventKind::MessageReceived(mut message) => {
            message.conversation_kind = ConversationKind::Group;
            WorldEvent::message_received(EventPriority::Normal, message)
        }
        _ => unreachable!(),
    };
    // Keep the explicit conversation scope supplied by the canonical event constructor.
    assert_eq!(
        group.scope(),
        EventScope::Conversation {
            conversation_id: conversation
        }
    );
    let group_request = MindSnapshotRequest::for_event(
        &group,
        None,
        MindSnapshotLimits::default(),
        MindInfluenceMode::Shadow,
    )
    .expect("valid group request");
    let group_snapshot = provider
        .snapshot(&group_request)
        .await
        .expect("group snapshot");
    assert!(
        group_snapshot
            .beliefs()
            .iter()
            .all(|belief| belief.scope != MindScope::Person { person_id: person })
    );
}

#[tokio::test]
async fn nonempty_snapshot_query_returns_only_relevant_state_or_no_opinion() {
    let store = Arc::new(InMemoryMindStore::new());
    let services = MindServices::from_store(Arc::clone(&store));
    let belief = Belief::new(
        BeliefId::new(),
        MindScope::Global,
        "Rust 类型系统有价值",
        0.9,
        0.8,
        BeliefSource::Seed,
        Vec::new(),
        None,
        now(),
    )
    .expect("belief");
    let preference = Preference::new(
        PreferenceId::new(),
        "Rust 类型系统",
        0.8,
        0.8,
        0.8,
        0.7,
        PreferenceSource::Experience,
        now(),
    )
    .expect("preference");
    let interest = Interest::new(
        InterestId::new(),
        "Rust 类型系统",
        0.9,
        0.8,
        0.6,
        MindSource::Experience,
        now(),
    )
    .expect("interest");
    services
        .beliefs
        .put(&belief, None)
        .await
        .expect("belief put");
    services
        .preferences
        .put(&preference, None)
        .await
        .expect("preference put");
    services
        .interests
        .put(&interest, None)
        .await
        .expect("interest put");

    let relevant = direct_event(
        PersonId::new(),
        ConversationId::new(),
        "你怎么看 Rust 类型系统？",
    );
    let relevant_request = MindSnapshotRequest::for_event(
        &relevant,
        None,
        MindSnapshotLimits::default(),
        MindInfluenceMode::Active,
    )
    .expect("relevant request");
    let provider = MindSnapshotStoreProvider::new(services.clone());
    let snapshot = provider
        .snapshot(&relevant_request)
        .await
        .expect("relevant snapshot");
    assert_eq!(snapshot.beliefs().len(), 1);
    assert_eq!(snapshot.preferences().len(), 1);
    assert_eq!(snapshot.interests().len(), 1);

    let unrelated = direct_event(
        PersonId::new(),
        ConversationId::new(),
        "烘焙面包时温度应该设多少？",
    );
    let unrelated_request = MindSnapshotRequest::for_event(
        &unrelated,
        None,
        MindSnapshotLimits::default(),
        MindInfluenceMode::Active,
    )
    .expect("unrelated request");
    let no_opinion = provider
        .snapshot(&unrelated_request)
        .await
        .expect("no-opinion snapshot");
    assert!(no_opinion.beliefs().is_empty());
    assert!(no_opinion.preferences().is_empty());
    assert!(no_opinion.interests().is_empty());

    assert_eq!(
        services
            .beliefs
            .relevant(&[MindScope::Global], "", now(), 8)
            .await
            .expect("maintenance belief listing")
            .len(),
        1,
        "empty maintenance queries must retain salience-based listing semantics"
    );
}

#[tokio::test]
async fn consolidation_clamps_updates_and_rejects_stale_snapshot() {
    let store = Arc::new(InMemoryMindStore::new());
    let services = MindServices::from_store(Arc::clone(&store));
    let base = services
        .consolidation
        .current_version()
        .await
        .expect("version");
    let event = direct_event(PersonId::new(), ConversationId::new(), "Rust");
    let input = ReflectionInput {
        trigger: ReflectionTrigger::HighSalienceEvent,
        depth: ReflectionDepth::Deep,
        scope: MindScope::Global,
        recent_events: Vec::new(),
        salient_memories: Vec::new(),
        open_loop_summaries: Vec::new(),
        goal_summaries: Vec::new(),
        mind: MindSnapshot::new(
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            MindInfluenceMode::Shadow,
            base,
            now(),
        )
        .expect("empty versioned snapshot"),
        requested_at: now(),
        trace: event.trace(),
    };
    let mut proposal = ReflectionProposal::empty(&input);
    proposal.belief_updates.push(BeliefUpdateProposal {
        operation: BeliefOperation::Upsert,
        belief_id: None,
        expected_version: None,
        scope: MindScope::Global,
        proposition: "Rust 类型系统总体有价值".to_owned(),
        confidence_delta: 1.0,
        stability_delta: 1.0,
        source: BeliefSource::Reflection,
        evidence_refs: Vec::new(),
        valid_until: None,
    });
    let consolidation = Consolidation::new(ConsolidationConfig::default()).expect("config");
    let result = consolidation
        .consolidate(&services, &proposal)
        .await
        .expect("consolidation");
    assert_eq!(result.applied_updates, 1);
    let stored = services
        .beliefs
        .find_by_key(
            MindScope::Global,
            &super::common::normalized_key("Rust 类型系统总体有价值"),
        )
        .await
        .expect("lookup")
        .expect("stored belief");
    assert!((stored.confidence() - 0.7).abs() < 0.001);
    assert!((stored.stability() - 0.45).abs() < 0.001);
    assert!(
        consolidation
            .consolidate(&services, &proposal)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn concurrent_updates_accept_only_one_expected_version() {
    let store = Arc::new(InMemoryMindStore::new());
    let belief = Belief::new(
        BeliefId::new(),
        MindScope::Global,
        "并发更新测试",
        0.5,
        0.5,
        BeliefSource::Seed,
        Vec::new(),
        None,
        now(),
    )
    .expect("valid belief");
    BeliefStore::put(store.as_ref(), &belief, None)
        .await
        .expect("initial insert");
    let first = belief
        .apply_delta(0.1, 0.0, &[], now() + Duration::seconds(1))
        .expect("update one");
    let second = belief
        .apply_delta(-0.1, 0.0, &[], now() + Duration::seconds(1))
        .expect("update two");
    let (left, right) = tokio::join!(
        BeliefStore::put(store.as_ref(), &first, Some(1)),
        BeliefStore::put(store.as_ref(), &second, Some(1)),
    );
    assert_ne!(left.is_ok(), right.is_ok());
}

#[tokio::test]
async fn erasure_removes_scoped_mind_data_but_keeps_global_self_state() {
    let store = Arc::new(InMemoryMindStore::new());
    let services = MindServices::from_store(Arc::clone(&store));
    services
        .self_model
        .put(&SelfModel::seed_yunxi(now()), None)
        .await
        .expect("seed self model");
    let person = PersonId::new();
    let belief = Belief::new(
        BeliefId::new(),
        MindScope::Person { person_id: person },
        "用户当前在找工作",
        0.7,
        0.4,
        BeliefSource::Conversation,
        Vec::new(),
        Some(now() + Duration::days(30)),
        now(),
    )
    .expect("person belief");
    services
        .beliefs
        .put(&belief, None)
        .await
        .expect("store belief");
    services
        .data_erasure
        .erase_person(person)
        .await
        .expect("erase person");
    assert!(services.beliefs.get(belief.id()).await.unwrap().is_none());
    assert!(services.self_model.get().await.unwrap().is_some());
}

#[test]
fn reflection_queue_is_bounded_and_coalesces_scope() {
    let queue = ReflectionQueue::new(ReflectionQueueConfig { capacity: 1 }).expect("queue");
    let event = direct_event(PersonId::new(), ConversationId::new(), "hi");
    let make_input = |at| ReflectionInput {
        trigger: ReflectionTrigger::Maintenance,
        depth: ReflectionDepth::Light,
        scope: MindScope::Global,
        recent_events: Vec::new(),
        salient_memories: Vec::new(),
        open_loop_summaries: Vec::new(),
        goal_summaries: Vec::new(),
        mind: MindSnapshot::empty(),
        requested_at: at,
        trace: event.trace(),
    };
    assert!(queue.enqueue(make_input(now())).expect("first enqueue"));
    assert!(
        !queue
            .enqueue(make_input(now() + Duration::minutes(1)))
            .expect("coalesced enqueue")
    );
    assert_eq!(queue.len(), 1);
    assert_eq!(
        queue.dequeue().expect("queued item").requested_at,
        now() + Duration::minutes(1)
    );
}

#[test]
fn reflection_queue_purges_erased_scopes_only() {
    let queue = ReflectionQueue::new(ReflectionQueueConfig { capacity: 4 }).expect("queue");
    let event = direct_event(PersonId::new(), ConversationId::new(), "hi");
    let person = PersonId::new();
    let conversation = ConversationId::new();
    let make_input = |scope| ReflectionInput {
        trigger: ReflectionTrigger::Maintenance,
        depth: ReflectionDepth::Light,
        scope,
        recent_events: Vec::new(),
        salient_memories: Vec::new(),
        open_loop_summaries: Vec::new(),
        goal_summaries: Vec::new(),
        mind: MindSnapshot::empty(),
        requested_at: now(),
        trace: event.trace(),
    };
    queue
        .enqueue(make_input(MindScope::Global))
        .expect("global enqueue");
    queue
        .enqueue(make_input(MindScope::Person { person_id: person }))
        .expect("person enqueue");
    queue
        .enqueue(make_input(MindScope::Conversation {
            conversation_id: conversation,
        }))
        .expect("conversation enqueue");

    assert_eq!(
        queue.purge_scopes(&[
            MindScope::Person { person_id: person },
            MindScope::Conversation {
                conversation_id: conversation,
            },
        ]),
        2
    );
    assert_eq!(queue.len(), 1);
    assert_eq!(
        queue.dequeue().expect("global remains").scope,
        MindScope::Global
    );
}
