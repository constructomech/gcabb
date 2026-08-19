//! Follow-up delivery coverage.
//!
//! GCABB owns the queue, so these tests pin the behaviour of the last mile:
//! that a follow-up reaches the agent as a turn, that it carries the mode it
//! was queued under, and that a failure is reported rather than swallowed.

use std::sync::Arc;

use app_model::QueueDelivery;
use copilot_provider::{AgentProvider, QueueDeliveryRequest};
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
async fn a_follow_up_reaches_the_agent_as_a_turn() {
    let (provider, session) = provider_with_session().await;

    let receipt = provider
        .deliver_queued(&session, &queued("first", QueueDelivery::WhenIdle))
        .await
        .expect("delivers");

    // Nothing is left holding the prompt on the runtime side: GCABB sends it
    // outright once it has decided the follow-up should run.
    assert!(receipt.message_id.is_some());
    assert_eq!(provider.sent_prompts().await, vec!["first".to_owned()]);
}

#[tokio::test]
async fn steering_and_queued_delivery_both_reach_the_agent() {
    let (provider, session) = provider_with_session().await;

    provider
        .deliver_queued(&session, &queued("interrupt", QueueDelivery::Steer))
        .await
        .expect("delivers");
    provider
        .deliver_queued(&session, &queued("later", QueueDelivery::WhenIdle))
        .await
        .expect("delivers");

    let deliveries = provider.delivered_queue(&session).await;
    let deliveries: Vec<_> = deliveries
        .iter()
        .map(|request| (request.prompt.as_str(), request.delivery))
        .collect();
    assert_eq!(
        deliveries,
        vec![
            ("interrupt", QueueDelivery::Steer),
            ("later", QueueDelivery::WhenIdle),
        ]
    );
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
    assert!(provider.delivered_queue(&session).await.is_empty());
}
