//! Queue delivery transport coverage.
//!
//! GCABB owns the queue, so these tests pin the behaviour of the last mile:
//! which transport a session resolves to, how an item reaches the agent under
//! each one, and that the capability GCABB cannot give up — interrupting a
//! running turn — survives the fallback.

use std::sync::Arc;

use app_model::QueueDelivery;
use copilot_provider::{AgentProvider, QueueDeliveryRequest, QueueTransportKind};
use test_harness::FakeProvider;

fn queued(prompt: &str, delivery: QueueDelivery) -> QueueDeliveryRequest {
    QueueDeliveryRequest {
        prompt: prompt.to_owned(),
        display_prompt: None,
        delivery,
        agent_mode: None,
    }
}

async fn provider_with_session() -> (Arc<FakeProvider>, String) {
    let provider = Arc::new(FakeProvider::default());
    provider.start().await.expect("provider starts");
    let session = provider
        .create_session(copilot_provider::SessionRequest::default())
        .await
        .expect("session created");
    let id = session.sdk_session_id.clone();
    (provider, id)
}

#[tokio::test]
async fn queued_items_enter_the_runtime_queue_when_it_is_available() {
    let (provider, session) = provider_with_session().await;

    let receipt = provider
        .deliver_queued(&session, &queued("first", QueueDelivery::WhenIdle))
        .await
        .expect("delivers");

    assert_eq!(
        provider.queue_transport(&session).await.unwrap(),
        QueueTransportKind::Native
    );
    // An item held by the runtime has a runtime id and has not been sent as a
    // turn, which is what lets it still be edited or withdrawn.
    assert!(receipt.runtime_id.is_some());
    assert!(receipt.message_id.is_none());
    let runtime = provider.runtime_queue(&session).await.expect("queue");
    assert_eq!(runtime.items.len(), 1);
    assert_eq!(runtime.items[0].display_text, "first");
    assert!(provider.sent_prompts().await.is_empty());
}

#[tokio::test]
async fn steering_reaches_the_agent_as_a_turn_rather_than_a_queue_entry() {
    let (provider, session) = provider_with_session().await;

    let receipt = provider
        .deliver_queued(&session, &queued("interrupt", QueueDelivery::Steer))
        .await
        .expect("delivers");

    // Steering has to reach a turn that is already running, so it must not sit
    // in the queue waiting to be drained.
    assert!(receipt.runtime_id.is_none());
    assert!(receipt.message_id.is_some());
    assert!(
        provider
            .runtime_queue(&session)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    assert_eq!(provider.sent_prompts().await, vec!["interrupt".to_owned()]);
}

#[tokio::test]
async fn a_runtime_without_a_queue_falls_back_to_sending() {
    let (provider, session) = provider_with_session().await;
    provider.without_runtime_queue(true);

    let receipt = provider
        .deliver_queued(&session, &queued("fallback", QueueDelivery::WhenIdle))
        .await
        .expect("delivers");

    assert_eq!(
        provider.queue_transport(&session).await.unwrap(),
        QueueTransportKind::SendOnIdle
    );
    assert!(receipt.runtime_id.is_none());
    assert_eq!(provider.sent_prompts().await, vec!["fallback".to_owned()]);
}

#[tokio::test]
async fn steering_still_works_without_a_runtime_queue() {
    let (provider, session) = provider_with_session().await;
    provider.without_runtime_queue(true);

    let receipt = provider
        .deliver_queued(&session, &queued("interrupt", QueueDelivery::Steer))
        .await
        .expect("delivers");

    // The capability GCABB cannot give up: losing the experimental queue
    // surface must not cost the ability to interrupt a running turn.
    assert!(receipt.message_id.is_some());
    assert_eq!(provider.sent_prompts().await, vec!["interrupt".to_owned()]);
}

#[tokio::test]
async fn withdrawing_removes_only_the_named_runtime_item() {
    let (provider, session) = provider_with_session().await;
    let first = provider
        .deliver_queued(&session, &queued("first", QueueDelivery::WhenIdle))
        .await
        .expect("delivers")
        .runtime_id
        .expect("runtime id");
    provider
        .deliver_queued(&session, &queued("second", QueueDelivery::WhenIdle))
        .await
        .expect("delivers");

    assert!(provider.withdraw_queued(&session, &first).await.unwrap());
    assert!(!provider.withdraw_queued(&session, &first).await.unwrap());

    let runtime = provider.runtime_queue(&session).await.expect("queue");
    let texts: Vec<_> = runtime
        .items
        .iter()
        .map(|item| item.display_text.as_str())
        .collect();
    assert_eq!(texts, vec!["second"]);
}

#[tokio::test]
async fn withdrawing_reports_nothing_to_do_without_a_runtime_queue() {
    let (provider, session) = provider_with_session().await;
    provider.without_runtime_queue(true);

    // Nothing is held by the runtime, so withdrawal is a no-op rather than an
    // error: GCABB's own queue is the thing being edited.
    assert!(!provider.withdraw_queued(&session, "0").await.unwrap());
}

#[tokio::test]
async fn pausing_is_recorded_against_the_session() {
    let (provider, session) = provider_with_session().await;
    assert!(!provider.queue_paused(&session).await);

    provider.set_queue_paused(&session, true).await.unwrap();
    assert!(provider.queue_paused(&session).await);

    provider.set_queue_paused(&session, false).await.unwrap();
    assert!(!provider.queue_paused(&session).await);
}

#[tokio::test]
async fn delivery_failures_surface_to_the_caller() {
    let (provider, session) = provider_with_session().await;
    provider.fail_queue_delivery(true);

    let error = provider
        .deliver_queued(&session, &queued("doomed", QueueDelivery::WhenIdle))
        .await
        .expect_err("delivery fails");

    assert!(error.to_string().contains("configured queue failure"));
    assert!(
        provider
            .runtime_queue(&session)
            .await
            .unwrap()
            .items
            .is_empty()
    );
}
