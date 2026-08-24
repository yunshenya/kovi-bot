//! Host-side prospective-memory scheduler.
//!
//! This task only claims due Core records and submits a platform-neutral
//! `ProspectiveMemoryDue` event. It deliberately has no bot, model, or QQ
//! action capability.

use chrono::{DateTime, Duration, Utc};
use std::sync::Arc;
use yunxi_core::{
    Admission, EventPriority, EventScope, OpenLoop, OpenLoopOwner, OpenLoopStore, RuntimeHandle,
    WorldEvent, WorldEventKind,
};

pub(crate) const OPEN_LOOP_CLAIM_BATCH: usize = 32;
const OPEN_LOOP_POLL_INTERVAL_SECS: u64 = 30;
const OPEN_LOOP_RETRY_DELAY_SECS: i64 = 60;

pub(crate) fn start(store: Arc<dyn OpenLoopStore>, runtime: RuntimeHandle) {
    kovi::tokio::spawn(run(store, runtime));
}

async fn run(store: Arc<dyn OpenLoopStore>, runtime: RuntimeHandle) {
    loop {
        let now = Utc::now();
        if let Err(error) = store
            .recover_stale_triggered(now, OPEN_LOOP_CLAIM_BATCH)
            .await
        {
            kovi::log::warn!("Yunxi open-loop stale-claim recovery failed: {error}");
        }

        match store.claim_due(now, OPEN_LOOP_CLAIM_BATCH).await {
            Ok(items) => {
                let mut runtime_closed = false;
                for item in items {
                    if runtime_closed {
                        if let Err(error) = store
                            .defer(
                                item.id(),
                                Some(now + Duration::seconds(OPEN_LOOP_RETRY_DELAY_SECS)),
                                Utc::now(),
                            )
                            .await
                        {
                            kovi::log::warn!(
                                "Yunxi open-loop retry defer failed for {}: {error}",
                                item.id()
                            );
                        }
                        continue;
                    }
                    match submit_due(&runtime, &item, now).await {
                        Ok(()) => {}
                        Err(error) => {
                            kovi::log::warn!(
                                "Yunxi open-loop due event could not be submitted: {error:?}"
                            );
                            let retry_at =
                                Some(now + Duration::seconds(OPEN_LOOP_RETRY_DELAY_SECS));
                            if let Err(defer_error) =
                                store.defer(item.id(), retry_at, Utc::now()).await
                            {
                                kovi::log::warn!(
                                    "Yunxi open-loop retry defer failed for {}: {defer_error}",
                                    item.id()
                                );
                            }
                            if matches!(error, DueSubmitError::RuntimeClosed) {
                                runtime_closed = true;
                            }
                        }
                    }
                }
                if runtime_closed {
                    return;
                }
            }
            Err(error) => {
                kovi::log::warn!("Yunxi open-loop due claim failed: {error}");
            }
        }

        kovi::tokio::time::sleep(kovi::tokio::time::Duration::from_secs(
            OPEN_LOOP_POLL_INTERVAL_SECS,
        ))
        .await;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DueSubmitError {
    RuntimeClosed,
    Dropped,
}

async fn submit_due(
    runtime: &RuntimeHandle,
    item: &OpenLoop,
    now: DateTime<Utc>,
) -> Result<(), DueSubmitError> {
    let event = WorldEvent::new(
        now,
        event_scope(item.owner()),
        EventPriority::High,
        WorldEventKind::ProspectiveMemoryDue(yunxi_core::ProspectiveMemoryEvent {
            open_loop_id: item.id(),
        }),
    );
    match runtime.submit(event).await {
        Ok(Admission::Accepted) => Ok(()),
        Ok(Admission::DroppedAtCapacity) => Err(DueSubmitError::Dropped),
        Err(yunxi_core::SubmitError::RuntimeClosed(_)) => Err(DueSubmitError::RuntimeClosed),
        Err(yunxi_core::SubmitError::InvalidEvent { .. }) => Err(DueSubmitError::Dropped),
    }
}

#[must_use]
pub(crate) const fn event_scope(owner: OpenLoopOwner) -> EventScope {
    match owner {
        OpenLoopOwner::Person(person_id) => EventScope::Person { person_id },
        OpenLoopOwner::Conversation(conversation_id) => {
            EventScope::Conversation { conversation_id }
        }
        OpenLoopOwner::Global => EventScope::Global,
    }
}

#[cfg(test)]
mod tests {
    use super::{event_scope, submit_due};
    use chrono::Utc;
    use yunxi_core::{
        ConversationId, EventPriority, EventScope, OpenLoop, OpenLoopKind, OpenLoopOwner, PersonId,
        ProcessingOutcome, RuntimeConfig,
    };

    #[test]
    fn due_scope_follows_core_owner_without_platform_ids() {
        let person_id = PersonId::new();
        let conversation_id = ConversationId::new();
        assert_eq!(
            event_scope(OpenLoopOwner::Person(person_id)),
            EventScope::Person { person_id }
        );
        assert_eq!(
            event_scope(OpenLoopOwner::Conversation(conversation_id)),
            EventScope::Conversation { conversation_id }
        );
        assert_eq!(event_scope(OpenLoopOwner::Global), EventScope::Global);
    }

    #[test]
    fn due_submission_only_enters_the_core_runtime() {
        let runtime = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        runtime.block_on(async {
            let draft = yunxi_core::OpenLoopDraft::new(
                OpenLoopOwner::Person(PersonId::new()),
                OpenLoopKind::AwaitingOutcome,
                "interview",
            )
            .expect("valid draft")
            .with_due_at(Some(Utc::now()));
            let item = OpenLoop::from_draft(yunxi_core::OpenLoopId::new(), &draft, Utc::now())
                .expect("valid open loop");
            let (handle, mut cognitive_runtime) =
                yunxi_core::CognitiveRuntime::new(RuntimeConfig::default()).expect("runtime");
            assert!(submit_due(&handle, &item, Utc::now()).await.is_ok());
            let Some(ProcessingOutcome::Observed(observation)) =
                cognitive_runtime.process_next().await
            else {
                panic!("due event should be observed by Core");
            };
            assert_eq!(observation.priority, EventPriority::High);
            assert_eq!(
                observation.event_type,
                yunxi_core::EventType::ProspectiveMemoryDue
            );
        });
    }
}
