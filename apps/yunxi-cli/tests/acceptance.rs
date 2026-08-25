use std::fs::OpenOptions;
use std::future::Future;
use std::io::Write;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::thread;

use yunxi_cli::{
    CliCoreState, CliError, CliHost, CliJournal, FakeEnvironment, FakeModel, HostResponse,
    JournalRecord, MAX_CLI_OPEN_LOOPS_PER_OWNER, MAX_JOURNAL_INPUT_BYTES,
};
use yunxi_core::{
    ConversationId, OpenLoopDraft, OpenLoopKind, OpenLoopOwner, OpenLoopStore, ProposedAction,
};

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
