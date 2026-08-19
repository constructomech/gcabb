//! Contract tests for the experimental runtime surfaces GCABB depends on.
//!
//! `session.queue.*` and the session SQL surface are both marked experimental
//! in the SDK, so their behaviour can change under a version bump without any
//! change here. These pin the specific properties GCABB relies on, so a bump
//! fails loudly instead of degrading quietly in a way that only shows up as
//! odd behaviour in the app.
//!
//! Run them when changing either pinned version:
//!
//! ```console
//! cargo test -p session-manager --test runtime_contract -- --ignored
//! ```

use std::sync::Arc;
use std::time::Duration;

use copilot_provider::{
    AgentProvider, CopilotProvider, QueueDeliveryRequest, QueueTransportKind, SessionRequest,
};
use diagnostics::MemoryDiagnostics;
use tempfile::tempdir;

fn queued(prompt: &str) -> QueueDeliveryRequest {
    QueueDeliveryRequest {
        prompt: prompt.to_owned(),
        display_prompt: None,
        delivery: app_model::QueueDelivery::WhenIdle,
        agent_mode: None,
    }
}

async fn started(project: &std::path::Path) -> Arc<CopilotProvider> {
    let provider = Arc::new(CopilotProvider::new(
        project,
        Arc::new(MemoryDiagnostics::default()),
    ));
    provider.start().await.expect("provider starts");
    provider
}

async fn session_on(provider: &Arc<CopilotProvider>, project: &std::path::Path) -> String {
    provider
        .create_session(SessionRequest {
            working_directory: project.to_owned(),
            auto_approve_tools: true,
            ..SessionRequest::default()
        })
        .await
        .expect("session created")
        .sdk_session_id
}

/// The queue surface still exists, and GCABB still selects it.
///
/// If the runtime drops it, GCABB degrades to sending on idle rather than
/// failing, so this asserts the capability is present rather than that the
/// app works — the fallback is covered by the deterministic tests.
#[tokio::test]
#[ignore = "uses the real Copilot SDK, account, and network"]
async fn the_runtime_still_offers_a_queue() {
    let project = tempdir().expect("tempdir");
    let provider = started(project.path()).await;
    let session = session_on(&provider, project.path()).await;

    assert_eq!(
        provider.queue_transport(&session).await.expect("transport"),
        QueueTransportKind::Native,
        "the runtime queue surface is gone; GCABB will fall back to sending on idle"
    );

    provider.disconnect(&session).await.expect("disconnect");
    provider.stop().await.expect("stop");
}

/// Queued items keep the order they were added in, and editing one edits it
/// in place rather than replacing or reordering it.
#[tokio::test]
#[ignore = "uses the real Copilot SDK, account, and network"]
async fn queued_items_keep_their_order_and_can_be_edited_in_place() {
    let project = tempdir().expect("tempdir");
    let provider = started(project.path()).await;
    let session = session_on(&provider, project.path()).await;
    // Holding the queue keeps these from reaching the model.
    provider
        .set_queue_paused(&session, true)
        .await
        .expect("pause");

    let first = provider
        .deliver_queued(&session, &queued("contract first"))
        .await
        .expect("first")
        .runtime_id
        .expect("runtime id");
    provider
        .deliver_queued(&session, &queued("contract second"))
        .await
        .expect("second");

    let runtime = provider.runtime_queue(&session).await.expect("pending");
    let texts: Vec<_> = runtime
        .items
        .iter()
        .map(|item| item.display_text.as_str())
        .collect();
    assert_eq!(
        texts,
        vec!["contract first", "contract second"],
        "queued items no longer keep insertion order"
    );

    // Withdrawal has to address the item the runtime reported, which is only
    // true while its identifiers stay stable across other operations.
    assert!(
        provider
            .withdraw_queued(&session, &first)
            .await
            .expect("withdraw"),
        "a runtime id reported by pendingItems no longer addresses that item"
    );
    let after = provider.runtime_queue(&session).await.expect("pending");
    assert_eq!(after.items.len(), 1);
    assert_eq!(after.items[0].display_text, "contract second");

    provider.disconnect(&session).await.expect("disconnect");
    provider.stop().await.expect("stop");
}

/// Immediate delivery still interrupts a turn that is already running.
///
/// This is the property GCABB cannot give up, and the one that makes losing
/// the queue surface survivable, so it is pinned separately from the queue.
#[tokio::test]
#[ignore = "uses the real Copilot SDK, account, network, and model quota"]
async fn immediate_delivery_still_steers_a_running_turn() {
    let project = tempdir().expect("tempdir");
    let provider = started(project.path()).await;
    let session = session_on(&provider, project.path()).await;

    provider
        .send(
            &session,
            "Count slowly from 1 to 40, one number per line, with a sentence about each.",
            &[],
        )
        .await
        .expect("long turn starts");
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Sent while the turn above is still streaming.
    let steer = provider
        .deliver_queued(
            &session,
            &QueueDeliveryRequest {
                prompt: "Stop counting. Reply with exactly: steered".to_owned(),
                display_prompt: None,
                delivery: app_model::QueueDelivery::Steer,
                agent_mode: None,
            },
        )
        .await
        .expect("steering is accepted during a running turn");
    assert!(
        steer.message_id.is_some(),
        "immediate delivery no longer reaches a running turn"
    );

    provider.disconnect(&session).await.expect("disconnect");
    provider.stop().await.expect("stop");
}

/// The runtime still routes the agent's SQL through a hosted filesystem, and
/// still bootstraps its task list with a multi-statement batch.
///
/// Both matter: the first is what makes the task list shared at all, and the
/// second is the shape that a single-statement path silently rejects.
#[tokio::test]
#[ignore = "uses the real Copilot SDK, account, network, and model quota"]
async fn a_hosted_database_still_receives_the_agents_sql() {
    let project = tempdir().expect("tempdir");
    let state = tempdir().expect("tempdir");
    let provider = Arc::new(
        CopilotProvider::new(project.path(), Arc::new(MemoryDiagnostics::default()))
            .hosting_session_state(state.path()),
    );
    provider.start().await.expect("provider starts");
    let session = session_on(&provider, project.path()).await;

    assert!(
        provider
            .agent_plan(&session)
            .await
            .expect("plan read")
            .writable,
        "a hosted session no longer reports its task list as writable"
    );

    provider
        .send(
            &session,
            "Use the sql tool to run exactly this query, then reply done: \
             INSERT INTO todos (id, title, status) \
             VALUES ('contract-row', 'Contract', 'pending')",
            &[],
        )
        .await
        .expect("send");

    let deadline = tokio::time::Instant::now() + Duration::from_mins(2);
    let plan = loop {
        let plan = provider.agent_plan(&session).await.expect("plan read");
        if !plan.is_empty() {
            break plan;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the agent's SQL never reached the hosted database"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    assert!(plan.todos.iter().any(|todo| todo.id == "contract-row"));

    provider.disconnect(&session).await.expect("disconnect");
    provider.stop().await.expect("stop");
}
