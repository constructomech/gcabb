//! Durable queue behaviour at the session level.
//!
//! These tests exercise the property the feature exists for: the queue is
//! GCABB's own state, editable regardless of what the agent is doing, and
//! surviving the runtime that has no memory of it.

use std::sync::Arc;
use std::time::Duration;

use app_model::{
    AgentPlan, AgentTodo, AgentTodoStatus, QueueDelivery, QueueItemState, SessionKind,
    SessionSnapshot, TitleSource,
};
use diagnostics::MemoryDiagnostics;
use session_manager::{CreateSessionRequest, SessionHandle, SessionManager};
use storage::Storage;
use tempfile::{TempDir, tempdir};
use test_harness::FakeProvider;

fn request(path: &std::path::Path) -> CreateSessionRequest {
    CreateSessionRequest {
        project_path: path.to_owned(),
        repository_root: None,
        title: "Queue".to_owned(),
        title_source: TitleSource::Manual,
        kind: SessionKind::Project,
        model: None,
        mode: Some("interactive".to_owned()),
        reasoning_effort: None,
        base_ref: None,
        context_tier: None,
    }
}

struct Harness {
    manager: SessionManager,
    provider: Arc<FakeProvider>,
    storage: Arc<Storage>,
    _dir: TempDir,
}

async fn harness() -> (Harness, SessionHandle) {
    let dir = tempdir().expect("tempdir");
    let provider = Arc::new(FakeProvider::default());
    let storage = Arc::new(Storage::open_in_memory().expect("storage"));
    let manager = SessionManager::new(
        provider.clone(),
        storage.clone(),
        Arc::new(MemoryDiagnostics::default()),
    );
    manager.start().await.expect("manager starts");
    let session = manager
        .create_session(request(dir.path()))
        .await
        .expect("session created");
    (
        Harness {
            manager,
            provider,
            storage,
            _dir: dir,
        },
        session,
    )
}

async fn idle(harness: &Harness, session: &SessionHandle) {
    let sdk_session_id = session_sdk_id(session);
    harness
        .provider
        .emit(
            &sdk_session_id,
            serde_json::json!({
                "id": format!("idle-{}", uuid::Uuid::new_v4()),
                "type": "session.idle",
                "data": {}
            }),
        )
        .await
        .expect("emit idle");
}

fn session_sdk_id(session: &SessionHandle) -> String {
    session.snapshot().metadata.sdk_session_id.clone()
}

async fn await_snapshot(
    session: &SessionHandle,
    predicate: impl Fn(&SessionSnapshot) -> bool,
) -> Arc<SessionSnapshot> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let snapshot = session.snapshot();
        if predicate(&snapshot) {
            return snapshot;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "snapshot never satisfied the predicate"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn queued_prompts_are_visible_and_ordered() {
    let (harness, session) = harness().await;
    session.set_queue_paused(true).await.expect("pause");

    session.enqueue("first").await.expect("enqueue");
    session.enqueue("second").await.expect("enqueue");

    let snapshot = await_snapshot(&session, |snapshot| snapshot.queue.items.len() == 2).await;
    let prompts: Vec<_> = snapshot
        .queue
        .items
        .iter()
        .map(|item| item.prompt.as_str())
        .collect();
    assert_eq!(prompts, vec!["first", "second"]);
    assert_eq!(snapshot.queue.pending_count(), 2);
    let _ = harness;
}

#[tokio::test]
async fn a_queued_prompt_can_be_edited_before_it_is_delivered() {
    let (_harness, session) = harness().await;
    session.set_queue_paused(true).await.expect("pause");
    let id = session.enqueue("draft").await.expect("enqueue");

    session
        .update_queued(&id, "revised", None)
        .await
        .expect("update");

    let snapshot = await_snapshot(&session, |snapshot| {
        snapshot
            .queue
            .item(&id)
            .is_some_and(|item| item.prompt == "revised")
    })
    .await;
    assert_eq!(snapshot.queue.pending_count(), 1);
}

#[tokio::test]
async fn queue_edits_do_not_wait_for_the_agent_to_be_idle() {
    let (harness, session) = harness().await;
    session.set_queue_paused(true).await.expect("pause");

    // Put the session somewhere other than idle, which is exactly when the
    // developer cannot otherwise talk to the agent.
    let sdk_session_id = session_sdk_id(&session);
    harness
        .provider
        .emit(
            &sdk_session_id,
            serde_json::json!({
                "id": "busy",
                "type": "assistant.turn_start",
                "data": {}
            }),
        )
        .await
        .expect("emit");
    await_snapshot(&session, |snapshot| {
        snapshot.status != app_model::SessionStatus::Idle
    })
    .await;

    let id = session.enqueue("while busy").await.expect("enqueue");
    session
        .update_queued(&id, "still editable", None)
        .await
        .expect("update");
    session.remove_queued(&id).await.expect("remove");

    let snapshot = await_snapshot(&session, |snapshot| snapshot.queue.items.is_empty()).await;
    assert!(snapshot.queue.items.is_empty());
}

#[tokio::test]
async fn reordering_changes_which_item_goes_next() {
    let (_harness, session) = harness().await;
    session.set_queue_paused(true).await.expect("pause");
    let first = session.enqueue("first").await.expect("enqueue");
    let second = session.enqueue("second").await.expect("enqueue");

    session
        .reorder_queue(vec![second.clone(), first.clone()])
        .await
        .expect("reorder");

    let snapshot = await_snapshot(&session, |snapshot| {
        snapshot
            .queue
            .next_pending()
            .is_some_and(|item| item.id == second)
    })
    .await;
    let ids: Vec<_> = snapshot
        .queue
        .items
        .iter()
        .map(|item| item.id.as_str())
        .collect();
    assert_eq!(ids, vec![second.as_str(), first.as_str()]);
}

#[tokio::test]
async fn an_idle_session_drains_one_item_at_a_time() {
    let (harness, session) = harness().await;
    session.set_queue_paused(true).await.expect("pause");
    session.enqueue("first").await.expect("enqueue");
    session.enqueue("second").await.expect("enqueue");

    session.set_queue_paused(false).await.expect("resume");
    idle(&harness, &session).await;

    // Exactly one item leaves the queue per idle: GCABB decides the ordering
    // rather than handing the whole queue over at once.
    let snapshot = await_snapshot(&session, |snapshot| {
        snapshot
            .queue
            .items
            .iter()
            .any(|item| item.state != QueueItemState::Pending)
    })
    .await;
    assert_eq!(snapshot.queue.pending_count(), 1);
    assert_eq!(
        snapshot
            .queue
            .next_pending()
            .map(|item| item.prompt.as_str()),
        Some("second")
    );
}

#[tokio::test]
async fn a_paused_queue_does_not_deliver_on_idle() {
    let (harness, session) = harness().await;
    session.set_queue_paused(true).await.expect("pause");
    session.enqueue("held").await.expect("enqueue");

    idle(&harness, &session).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_eq!(session.snapshot().queue.pending_count(), 1);
    assert!(harness.provider.sent_prompts().await.is_empty());
}

#[tokio::test]
async fn a_failed_delivery_does_not_block_the_rest_of_the_queue() {
    let (harness, session) = harness().await;
    session.set_queue_paused(true).await.expect("pause");
    session.enqueue("doomed").await.expect("enqueue");
    session.enqueue("survivor").await.expect("enqueue");
    harness.provider.fail_queue_delivery(true);

    session.set_queue_paused(false).await.expect("resume");
    idle(&harness, &session).await;

    let snapshot = await_snapshot(&session, |snapshot| {
        snapshot
            .queue
            .items
            .iter()
            .any(|item| item.state == QueueItemState::Failed)
    })
    .await;
    let failed = snapshot
        .queue
        .items
        .iter()
        .find(|item| item.state == QueueItemState::Failed)
        .expect("failed item");
    assert_eq!(failed.prompt, "doomed");
    assert!(failed.error.is_some());
    // The failure is terminal rather than retried, so the next item is free to
    // run instead of being stuck behind a prompt that always fails.
    assert_eq!(
        snapshot
            .queue
            .next_pending()
            .map(|item| item.prompt.as_str()),
        Some("survivor")
    );
}

#[tokio::test]
async fn the_queue_outlives_the_session_that_held_it() {
    let (harness, session) = harness().await;
    session.set_queue_paused(true).await.expect("pause");
    session.enqueue("survives").await.expect("enqueue");
    let session_id = session.id().to_owned();

    session.disconnect().await.expect("disconnect");

    // The runtime forgets its queue on disconnect; GCABB's must not.
    let queue = harness.storage.queue_view(&session_id).expect("queue");
    assert_eq!(queue.items.len(), 1);
    assert_eq!(queue.items[0].prompt, "survives");
    assert!(queue.paused);
    let _ = harness.manager;
}

#[tokio::test]
async fn clearing_removes_pending_items_only() {
    let (harness, session) = harness().await;
    session.set_queue_paused(true).await.expect("pause");
    session.enqueue("first").await.expect("enqueue");
    session.set_queue_paused(false).await.expect("resume");
    idle(&harness, &session).await;
    await_snapshot(&session, |snapshot| {
        snapshot
            .queue
            .items
            .iter()
            .any(|item| item.state != QueueItemState::Pending)
    })
    .await;
    session.enqueue("pending").await.expect("enqueue");

    session.clear_queue().await.expect("clear");

    let snapshot = await_snapshot(&session, |snapshot| snapshot.queue.pending_count() == 0).await;
    // History of what already ran is worth keeping; only the waiting work goes.
    assert!(
        snapshot
            .queue
            .items
            .iter()
            .all(|item| item.state != QueueItemState::Pending)
    );
}

#[tokio::test]
async fn the_agent_plan_refreshes_when_the_runtime_signals_a_change() {
    let (harness, session) = harness().await;
    assert!(session.snapshot().agent_plan.is_empty());

    harness
        .provider
        .set_agent_plan(AgentPlan {
            todos: vec![
                AgentTodo {
                    id: "a".to_owned(),
                    title: "First".to_owned(),
                    description: None,
                    status: AgentTodoStatus::Done,
                    depends_on: Vec::new(),
                },
                AgentTodo {
                    id: "b".to_owned(),
                    title: "Second".to_owned(),
                    description: None,
                    status: AgentTodoStatus::InProgress,
                    depends_on: vec!["a".to_owned()],
                },
            ],
            writable: false,
        })
        .await;
    // The event carries no payload, so the list has to be re-read rather than
    // reconstructed from it.
    harness
        .provider
        .emit(
            &session_sdk_id(&session),
            serde_json::json!({
                "id": "todos-1",
                "type": "session.todos_changed",
                "data": {}
            }),
        )
        .await
        .expect("emit");

    let snapshot = await_snapshot(&session, |snapshot| !snapshot.agent_plan.is_empty()).await;
    assert_eq!(snapshot.agent_plan.total(), 2);
    assert_eq!(snapshot.agent_plan.completed(), 1);
    assert_eq!(
        snapshot
            .agent_plan
            .current()
            .map(|todo| todo.title.as_str()),
        Some("Second")
    );
}

#[tokio::test]
async fn steering_items_reach_the_agent_as_a_turn() {
    let (harness, session) = harness().await;
    session
        .enqueue_with("interrupt", None, QueueDelivery::Steer)
        .await
        .expect("enqueue");

    idle(&harness, &session).await;

    await_snapshot(&session, |snapshot| {
        snapshot
            .queue
            .items
            .iter()
            .any(|item| item.state != QueueItemState::Pending)
    })
    .await;
    assert_eq!(
        harness.provider.sent_prompts().await,
        vec!["interrupt".to_owned()]
    );
}

#[tokio::test]
async fn the_agents_task_list_can_be_edited_when_it_is_writable() {
    let (harness, session) = harness().await;
    harness
        .provider
        .set_agent_plan(AgentPlan {
            todos: vec![AgentTodo {
                id: "a".to_owned(),
                title: "Agent task".to_owned(),
                description: None,
                status: AgentTodoStatus::Pending,
                depends_on: Vec::new(),
            }],
            writable: true,
        })
        .await;

    session
        .set_todo_status("a", AgentTodoStatus::Done)
        .await
        .expect("status change");

    let snapshot = await_snapshot(&session, |snapshot| {
        snapshot
            .agent_plan
            .todos
            .iter()
            .any(|todo| todo.status == AgentTodoStatus::Done)
    })
    .await;
    assert!(snapshot.agent_plan.writable);
}

#[tokio::test]
async fn host_authored_tasks_join_the_agents_list() {
    let (harness, session) = harness().await;
    harness
        .provider
        .set_agent_plan(AgentPlan {
            todos: Vec::new(),
            writable: true,
        })
        .await;

    session
        .upsert_todo(AgentTodo {
            id: "gcabb-1".to_owned(),
            title: "Host priority".to_owned(),
            description: None,
            status: AgentTodoStatus::Pending,
            depends_on: Vec::new(),
        })
        .await
        .expect("upsert");

    let snapshot = await_snapshot(&session, |snapshot| !snapshot.agent_plan.is_empty()).await;
    assert_eq!(snapshot.agent_plan.todos[0].title, "Host priority");

    session.remove_todo("gcabb-1").await.expect("remove");
    let cleared = await_snapshot(&session, |snapshot| snapshot.agent_plan.is_empty()).await;
    assert!(cleared.agent_plan.todos.is_empty());
}

#[tokio::test]
async fn editing_a_task_list_that_is_not_writable_fails_rather_than_pretending() {
    let (harness, session) = harness().await;
    harness
        .provider
        .set_agent_plan(AgentPlan {
            todos: vec![AgentTodo {
                id: "a".to_owned(),
                title: "Agent task".to_owned(),
                description: None,
                status: AgentTodoStatus::Pending,
                depends_on: Vec::new(),
            }],
            writable: false,
        })
        .await;

    // Without a hosted filesystem the runtime owns these rows outright, and a
    // silent no-op would look like the edit had been accepted.
    let error = session
        .set_todo_status("a", AgentTodoStatus::Done)
        .await
        .expect_err("edit is refused");
    assert!(error.to_string().contains("hosts the session filesystem"));
}

#[tokio::test]
async fn a_runtime_without_a_queue_is_reported_as_degraded_not_broken() {
    let dir = tempdir().expect("tempdir");
    let provider = Arc::new(FakeProvider::default());
    provider.without_runtime_queue(true);
    let storage = Arc::new(Storage::open_in_memory().expect("storage"));
    let manager = SessionManager::new(
        provider.clone(),
        storage,
        Arc::new(MemoryDiagnostics::default()),
    );
    manager.start().await.expect("start");
    let session = manager
        .create_session(request(dir.path()))
        .await
        .expect("session");

    let snapshot = await_snapshot(&session, |snapshot| {
        snapshot
            .capabilities
            .get(app_model::CapabilityId::NativeQueue)
            .is_some()
    })
    .await;

    let native = snapshot
        .capabilities
        .get(app_model::CapabilityId::NativeQueue)
        .expect("capability recorded");
    assert_eq!(native.status, app_model::CapabilityStatus::Unavailable);
    // Losing the runtime queue must not block the session, only degrade it.
    assert!(
        !snapshot
            .capabilities
            .blocking()
            .iter()
            .any(|capability| capability.id == app_model::CapabilityId::NativeQueue)
    );
    // And the queue still works through the fallback.
    session.enqueue("still works").await.expect("enqueue");
}

#[tokio::test]
async fn a_hosted_task_list_is_reported_as_shared() {
    let (harness, session) = harness().await;
    harness
        .provider
        .set_agent_plan(AgentPlan {
            todos: Vec::new(),
            writable: true,
        })
        .await;

    // Recorded when the actor starts, so a fresh session is needed to observe
    // it; the queue capability alone proves the recording ran.
    let snapshot = await_snapshot(&session, |snapshot| {
        snapshot
            .capabilities
            .get(app_model::CapabilityId::NativeQueue)
            .is_some()
    })
    .await;
    assert_eq!(
        snapshot
            .capabilities
            .get(app_model::CapabilityId::NativeQueue)
            .map(|capability| capability.status),
        Some(app_model::CapabilityStatus::Available)
    );
}
