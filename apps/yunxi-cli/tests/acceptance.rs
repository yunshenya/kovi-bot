use std::fs::OpenOptions;
use std::future::Future;
use std::io::Write;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::thread;

use chrono::{Duration, Utc};
use yunxi_cli::{
    CliCoreState, CliError, CliHost, CliJournal, FakeEnvironment, FakeModel, HostResponse,
    JournalRecord, MAX_CLI_OPEN_LOOPS_PER_OWNER, MAX_JOURNAL_INPUT_BYTES,
};
use yunxi_core::{
    AutonomyPolicy, ConversationId, ConversationTurnDirective, DecisionDisposition, DecisionPlan,
    MessageContent, ModelBackend as CoreModelBackend, OpenLoopDraft, OpenLoopKind, OpenLoopOwner,
    OpenLoopStore, PlannerInput, ProposedAction, WorldEventKind,
};

#[derive(Debug, Clone, Copy)]
struct TwoMessageModel;

impl CoreModelBackend for TwoMessageModel {
    fn plan<'a>(&'a self, input: &'a PlannerInput) -> yunxi_core::ModelBackendFuture<'a> {
        Box::pin(async move {
            let WorldEventKind::MessageReceived(message) = input.event.kind() else {
                return Ok(DecisionPlan::silent());
            };
            if message.content.as_text() != "给我发两条消息" {
                return Ok(DecisionPlan::silent());
            }
            Ok(DecisionPlan {
                disposition: DecisionDisposition::Reply,
                intents: vec![
                    yunxi_core::CognitiveIntent::send_message(
                        message.conversation_id,
                        MessageContent::text("第一条"),
                    ),
                    yunxi_core::CognitiveIntent::send_message(
                        message.conversation_id,
                        MessageContent::text("第二条"),
                    ),
                ],
                state_updates: Vec::new(),
            })
        })
    }
}

#[derive(Debug, Clone, Copy)]
struct SilentAutonomousModel;

impl CoreModelBackend for SilentAutonomousModel {
    fn plan<'a>(&'a self, input: &'a PlannerInput) -> yunxi_core::ModelBackendFuture<'a> {
        Box::pin(async move {
            if matches!(
                input.event.kind(),
                WorldEventKind::AutonomousConversationTick(_)
            ) {
                return Ok(DecisionPlan::silent());
            }
            let WorldEventKind::MessageReceived(message) = input.event.kind() else {
                return Ok(DecisionPlan::silent());
            };
            Ok(DecisionPlan {
                disposition: DecisionDisposition::Reply,
                intents: vec![yunxi_core::CognitiveIntent::send_message(
                    message.conversation_id,
                    MessageContent::text("初始回复"),
                )],
                state_updates: Vec::new(),
            })
        })
    }
}

#[test]
fn fake_model_and_environment_complete_a_core_action() {
    let environment = FakeEnvironment::default();
    let host = CliHost::new(FakeModel, environment, ConversationId::new());

    let response = host
        .process_line("hello from the standalone host")
        .expect("response");
    assert_eq!(
        response,
        HostResponse::Delivered {
            message: "Yunxi heard: hello from the standalone host".to_owned(),
            external_reference: Some("fake-delivery-1".to_owned()),
        }
    );

    let deliveries = host.environment().deliveries();
    assert_eq!(deliveries.len(), 1);
    let ProposedAction::SendMessage(action) = &deliveries[0] else {
        panic!("fake environment should receive a send-message action");
    };
    assert_eq!(action.conversation_id, host.conversation_id());
    assert_eq!(
        action.content.as_text(),
        "Yunxi heard: hello from the standalone host"
    );
}

#[test]
fn cli_delivers_each_message_in_a_multi_action_plan() {
    let environment = FakeEnvironment::default();
    let host = CliHost::new(TwoMessageModel, environment, ConversationId::new());

    let response = host
        .process_line("给我发两条消息")
        .expect("response should be delivered");
    assert!(matches!(
        response,
        HostResponse::Delivered { ref message, .. } if message == "第一条"
    ));

    let deliveries = host.environment().deliveries();
    assert_eq!(deliveries.len(), 2);
    let actions = deliveries
        .iter()
        .map(|delivery| match delivery {
            ProposedAction::SendMessage(action) => action,
            other => panic!("expected send-message action, got {other:?}"),
        })
        .collect::<Vec<_>>();
    assert_eq!(actions[0].content.as_text(), "第一条");
    assert_eq!(actions[1].content.as_text(), "第二条");
    assert_ne!(actions[0].idempotency_key(), actions[1].idempotency_key());
}

#[test]
fn noop_decision_does_not_call_the_environment() {
    let environment = FakeEnvironment::default();
    let host = CliHost::new(FakeModel, environment, ConversationId::new());

    assert_eq!(
        host.process_line("/noop").expect("response"),
        HostResponse::Noop
    );
    assert!(host.environment().deliveries().is_empty());
}

#[test]
fn autonomous_ticks_continue_a_direct_conversation_after_idle() {
    let start = Utc::now();
    let policy = AutonomyPolicy {
        direct_idle: Duration::seconds(1),
        direct_cooldown: Duration::seconds(1),
        group_idle: Duration::seconds(1),
        group_cooldown: Duration::seconds(1),
        ..AutonomyPolicy::default()
    };
    let host = CliHost::new(FakeModel, FakeEnvironment::default(), ConversationId::new())
        .try_with_autonomy_policy(policy)
        .expect("test policy should be valid");

    assert!(matches!(
        host.process_line_at("hello", start).expect("initial reply"),
        HostResponse::Delivered { .. }
    ));
    let lifecycle = host.lifecycle().expect("lifecycle snapshot");
    assert_eq!(lifecycle.directive(), ConversationTurnDirective::Continue);
    assert_eq!(lifecycle.autonomous_turns(), 0);

    assert!(
        host.process_autonomous_tick_at(start + Duration::milliseconds(500))
            .expect("early tick")
            .is_none()
    );
    let autonomous = host
        .process_autonomous_tick_at(start + Duration::seconds(2))
        .expect("autonomous tick")
        .expect("tick should be due");
    assert!(
        matches!(autonomous, HostResponse::Delivered { ref message, .. } if message.contains("kept thinking"))
    );
    let lifecycle = host.lifecycle().expect("lifecycle snapshot");
    assert_eq!(lifecycle.autonomous_turns(), 1);
    assert!(!lifecycle.is_in_flight());

    assert!(
        host.process_autonomous_tick_at(start + Duration::milliseconds(2_500))
            .expect("cooldown tick")
            .is_none()
    );
    let second = host
        .process_autonomous_tick_at(start + Duration::seconds(4))
        .expect("second autonomous tick")
        .expect("second tick should be due");
    assert!(
        matches!(second, HostResponse::Delivered { ref message, .. } if message.contains("pause here"))
    );
    assert_eq!(
        host.lifecycle()
            .expect("lifecycle snapshot")
            .autonomous_turns(),
        2
    );
    assert_eq!(
        host.lifecycle().expect("lifecycle snapshot").directive(),
        ConversationTurnDirective::End
    );
    assert!(
        host.process_autonomous_tick_at(start + Duration::hours(1))
            .expect("tick after autonomous end")
            .is_none()
    );
}

#[test]
fn a_silent_autonomous_turn_pauses_instead_of_hot_looping() {
    let start = Utc::now();
    let policy = AutonomyPolicy {
        direct_idle: Duration::seconds(1),
        direct_cooldown: Duration::seconds(1),
        ..AutonomyPolicy::default()
    };
    let host = CliHost::new(
        SilentAutonomousModel,
        FakeEnvironment::default(),
        ConversationId::new(),
    )
    .try_with_autonomy_policy(policy)
    .expect("test policy should be valid");
    assert!(matches!(
        host.process_line_at("hello", start).expect("initial reply"),
        HostResponse::Delivered { .. }
    ));
    assert!(matches!(
        host.process_autonomous_tick_at(start + Duration::seconds(2))
            .expect("silent autonomous tick"),
        Some(HostResponse::Noop)
    ));
    assert_eq!(
        host.lifecycle().expect("lifecycle snapshot").directive(),
        ConversationTurnDirective::Wait
    );
    assert!(
        host.process_autonomous_tick_at(start + Duration::hours(1))
            .expect("tick after pause")
            .is_none()
    );
}

#[test]
fn a_new_inbound_turn_resets_autonomous_due_state() {
    let start = Utc::now();
    let policy = AutonomyPolicy {
        direct_idle: Duration::seconds(1),
        direct_cooldown: Duration::seconds(1),
        ..AutonomyPolicy::default()
    };
    let host = CliHost::new(FakeModel, FakeEnvironment::default(), ConversationId::new())
        .try_with_autonomy_policy(policy)
        .expect("test policy should be valid");
    host.process_line_at("first", start).expect("first reply");
    host.process_autonomous_tick_at(start + Duration::seconds(2))
        .expect("autonomous turn")
        .expect("autonomous turn should be due");

    host.process_line_at(
        "new inbound",
        start + Duration::seconds(2) + Duration::milliseconds(100),
    )
    .expect("new reply");
    assert!(
        host.process_autonomous_tick_at(start + Duration::seconds(2) + Duration::milliseconds(200))
            .expect("tick after inbound")
            .is_none()
    );
    // A fresh inbound starts a new autonomous chain, so the previous burst
    // counter is reset rather than accumulated.
    assert_eq!(
        host.lifecycle()
            .expect("lifecycle snapshot")
            .autonomous_turns(),
        0
    );
}

#[test]
fn a_silent_reactive_turn_does_not_start_an_autonomous_loop() {
    let start = Utc::now();
    let policy = AutonomyPolicy {
        direct_idle: Duration::seconds(1),
        direct_cooldown: Duration::seconds(1),
        ..AutonomyPolicy::default()
    };
    let host = CliHost::new(FakeModel, FakeEnvironment::default(), ConversationId::new())
        .try_with_autonomy_policy(policy)
        .expect("test policy should be valid");
    assert_eq!(
        host.process_line_at("/noop", start).expect("noop"),
        HostResponse::Noop
    );
    assert!(
        host.process_autonomous_tick_at(start + Duration::hours(1))
            .expect("tick after noop")
            .is_none()
    );
}

#[test]
fn empty_input_is_ignored() {
    let environment = FakeEnvironment::default();
    let host = CliHost::new(FakeModel, environment, ConversationId::new());

    assert_eq!(
        host.process_line("  \t").expect("response"),
        HostResponse::Empty
    );
    assert!(host.environment().deliveries().is_empty());
}

#[test]
fn journal_persists_started_and_completed_turns_across_reopen() {
    let path = unique_journal_path("persist");
    let conversation_id = ConversationId::new();
    let journal = Arc::new(CliJournal::open(&path).expect("open journal"));
    let environment = FakeEnvironment::default();
    let host = CliHost::new(FakeModel, environment, conversation_id).with_journal(journal.clone());

    let response = host.process_line("durable hello").expect("response");
    assert!(matches!(response, HostResponse::Delivered { .. }));
    let records = journal.records().expect("read journal");
    assert_eq!(records.len(), 2);
    assert!(matches!(
        &records[0],
        JournalRecord::TurnStarted {
            sequence: 1,
            conversation_id: recorded_conversation,
            input,
            ..
        } if *recorded_conversation == conversation_id && input == "durable hello"
    ));
    assert!(matches!(
        &records[1],
        JournalRecord::TurnCompleted {
            sequence: 1,
            response: HostResponse::Delivered { .. },
            ..
        }
    ));

    drop(host);
    drop(journal);
    let reopened = CliJournal::open(&path).expect("reopen journal");
    assert_eq!(
        reopened
            .start(conversation_id, "second turn")
            .expect("start"),
        2
    );
    let records = reopened.records().expect("read reopened journal");
    assert_eq!(records.len(), 3);
    remove_journal(&path);
}

#[test]
fn journal_ignores_only_a_crash_truncated_tail() {
    let path = unique_journal_path("tail");
    let conversation_id = ConversationId::new();
    let journal = CliJournal::open(&path).expect("open journal");
    journal.start(conversation_id, "complete").expect("start");
    drop(journal);

    let mut file = OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("append truncated tail");
    file.write_all(br#"{"type":"turn_started""#)
        .expect("write truncated tail");
    drop(file);

    let reopened = CliJournal::open(&path).expect("reopen journal");
    assert_eq!(reopened.records().expect("read records").len(), 1);
    remove_journal(&path);
}

#[test]
fn journal_rejects_oversized_input_before_core_runs() {
    let path = unique_journal_path("limit");
    let journal = Arc::new(CliJournal::open(&path).expect("open journal"));
    let environment = FakeEnvironment::default();
    let host = CliHost::new(FakeModel, environment, ConversationId::new()).with_journal(journal);
    let input = "x".repeat(MAX_JOURNAL_INPUT_BYTES + 1);

    let error = host
        .process_line(&input)
        .expect_err("oversized input should fail");
    assert!(error.to_string().contains("journal input"));
    assert!(host.environment().deliveries().is_empty());
    remove_journal(&path);
}

#[test]
fn persistent_core_state_restores_model_context_and_open_loops() {
    let path = unique_state_path("context");
    let state = Arc::new(CliCoreState::open(&path).expect("open state"));
    let person_id = state.person_id();
    let conversation_id = state.conversation_id();
    let host = CliHost::new(FakeModel, FakeEnvironment::default(), ConversationId::new())
        .with_core_state(state.clone());

    assert_eq!(host.person_id(), person_id);
    assert_eq!(host.conversation_id(), conversation_id);
    assert_eq!(
        host.lifecycle()
            .expect("lifecycle snapshot")
            .conversation_id(),
        conversation_id
    );
    assert_eq!(
        host.process_line("first persistent turn")
            .expect("first turn"),
        HostResponse::Delivered {
            message: "Yunxi heard: first persistent turn".to_owned(),
            external_reference: Some("fake-delivery-1".to_owned()),
        }
    );
    let todo = host
        .process_line("/todo inspect persisted context")
        .expect("create open loop");
    assert!(matches!(
        todo,
        HostResponse::Delivered { ref message, .. }
            if message.contains("Yunxi noted: inspect persisted context")
    ));
    let stats = state.stats().expect("state stats");
    assert_eq!(stats.memories, 2);
    assert_eq!(stats.open_loops, 1);
    assert_eq!(stats.affects, 1);
    assert_eq!(stats.relations, 1);
    drop(host);
    drop(state);

    let reopened = Arc::new(CliCoreState::open(&path).expect("reopen state"));
    assert_eq!(reopened.person_id(), person_id);
    assert_eq!(reopened.conversation_id(), conversation_id);
    let host = CliHost::new(FakeModel, FakeEnvironment::default(), ConversationId::new())
        .with_core_state(reopened);
    let response = host.process_line("after restart").expect("restored turn");
    assert!(matches!(
        response,
        HostResponse::Delivered { ref message, .. }
            if message.contains("2 memories")
                && message.contains("1 open loops")
                && message.contains("familiarity 0.10")
    ));
    let closed = host.process_line("/done").expect("resolve open loop");
    assert!(matches!(
        closed,
        HostResponse::Delivered { ref message, .. }
            if message.contains("closed the next open item")
    ));
    let after_close = host
        .process_line("after close")
        .expect("turn after resolve");
    assert!(matches!(
        after_close,
        HostResponse::Delivered { ref message, .. }
            if message.contains("0 open loops")
    ));
    assert!(
        std::fs::metadata(&path).expect("state metadata").len()
            <= yunxi_cli::MAX_CLI_STATE_BYTES as u64
    );
    remove_journal(&path);
}

#[test]
fn secondary_open_loop_failure_is_not_reported_as_delivered() {
    let state = Arc::new(CliCoreState::in_memory_for(
        yunxi_core::PersonId::new(),
        ConversationId::new(),
    ));
    let owner = OpenLoopOwner::Conversation(state.conversation_id());
    for index in 0..MAX_CLI_OPEN_LOOPS_PER_OWNER {
        let draft = OpenLoopDraft::new(
            owner,
            OpenLoopKind::FollowUp,
            format!("existing item {index}"),
        )
        .expect("draft");
        block_on(state.create(&draft)).expect("fill open-loop capacity");
    }
    let host = CliHost::new(FakeModel, FakeEnvironment::default(), ConversationId::new())
        .with_core_state(state.clone());

    let error = host
        .process_line("/todo one item too many")
        .expect_err("secondary create failure must be surfaced");
    assert!(matches!(error, CliError::Port(_)));
    assert_eq!(host.environment().deliveries().len(), 1);
    assert_eq!(
        state.stats().expect("stats").open_loops,
        MAX_CLI_OPEN_LOOPS_PER_OWNER
    );
}

fn unique_journal_path(label: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "yunxi-cli-{label}-{}-{nanos}.jsonl",
        std::process::id()
    ))
}

fn unique_state_path(label: &str) -> std::path::PathBuf {
    let mut path = unique_journal_path(label);
    path.set_extension("json");
    path
}

fn remove_journal(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
}

fn block_on<F>(future: F) -> F::Output
where
    F: Future,
{
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
    let mut future = Box::pin(future);
    loop {
        match Pin::new(&mut future).poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => thread::yield_now(),
        }
    }
}
