use super::*;
use crate::{
    ConversationId, ConversationKind, EventPriority, EventScope, MessageContent, MessageId,
    MessageReceivedEvent, PersonId, WorldEvent,
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
