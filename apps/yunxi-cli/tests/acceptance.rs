use yunxi_cli::{CliHost, FakeEnvironment, FakeModel, HostResponse};
use yunxi_core::{ConversationId, ProposedAction};

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
