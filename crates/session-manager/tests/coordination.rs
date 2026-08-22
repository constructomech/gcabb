use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use app_model::{
    InteractionKind, InteractionRequest, ProjectMetadata, SessionKind, SessionLaunchOrigin,
    SessionMetadata, TitleSource,
};
use copilot_provider::{
    ChildLifecycleEvent, ChildLifecycleStatus, HostGatewayEvent, HostToolGateway,
    SessionMessageDelivery,
};
use diagnostics::MemoryDiagnostics;
use serde_json::json;
use session_manager::{CreateSessionRequest, SessionHandle, SessionManager};
use storage::{CoordinationKind, Storage};
use tempfile::TempDir;
use test_harness::FakeProvider;
use tokio::sync::mpsc;

fn project(path: &Path) -> ProjectMetadata {
    ProjectMetadata {
        id: "project".to_owned(),
        path: path.to_string_lossy().into_owned(),
        name: "Project".to_owned(),
        default_branch: Some("main".to_owned()),
        last_opened_at: "1".to_owned(),
    }
}

fn request(path: &Path, title: &str) -> CreateSessionRequest {
    CreateSessionRequest {
        project_path: path.to_owned(),
        title: title.to_owned(),
        title_source: TitleSource::Manual,
        model: None,
        mode: Some("autopilot".to_owned()),
        agent: None,
        reasoning_effort: None,
        context_tier: None,
        base_ref: Some("main".to_owned()),
        repository_root: Some(path.to_string_lossy().into_owned()),
        kind: SessionKind::Project,
        parent_session_id: None,
        launch_origin: SessionLaunchOrigin::User,
        host_tool_call_id: None,
        unattended: true,
    }
}

fn child_request(
    path: &Path,
    parent_session_id: &str,
    title: &str,
    tool_call_id: &str,
) -> CreateSessionRequest {
    CreateSessionRequest {
        parent_session_id: Some(parent_session_id.to_owned()),
        launch_origin: SessionLaunchOrigin::AgentTool,
        host_tool_call_id: Some(tool_call_id.to_owned()),
        ..request(path, title)
    }
}

async fn wait_for(
    session: &SessionHandle,
    predicate: impl Fn(&app_model::SessionSnapshot) -> bool,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !predicate(&session.snapshot()) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "session snapshot did not reach the expected state"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

struct Harness {
    directory: TempDir,
    manager: Arc<SessionManager>,
    provider: Arc<FakeProvider>,
    storage: Arc<Storage>,
    parent: SessionHandle,
}

async fn harness(with_gateway: bool) -> (Harness, Option<mpsc::Receiver<HostGatewayEvent>>) {
    let directory = tempfile::tempdir().expect("temporary project");
    let storage = Arc::new(Storage::open_in_memory().expect("storage"));
    let provider = Arc::new(FakeProvider::default());
    let (sender, receiver) = mpsc::channel(16);
    let mut manager = SessionManager::new(
        provider.clone(),
        storage.clone(),
        Arc::new(MemoryDiagnostics::default()),
    );
    if with_gateway {
        manager = manager.with_host_tool_gateway(HostToolGateway::new(sender));
    }
    let manager = Arc::new(manager);
    manager
        .register_project(&project(directory.path()))
        .expect("register project");
    let parent = manager
        .create_session(request(directory.path(), "Parent"))
        .await
        .expect("create parent");
    (
        Harness {
            directory,
            manager,
            provider,
            storage,
            parent,
        },
        with_gateway.then_some(receiver),
    )
}

#[tokio::test]
async fn get_session_is_bounded_and_includes_plan_and_change_metadata() {
    let (harness, _) = harness(false).await;
    let child = harness
        .manager
        .create_session(child_request(
            harness.directory.path(),
            harness.parent.id(),
            "Child",
            "create-child",
        ))
        .await
        .expect("create child");
    let sdk_id = child.snapshot().metadata.sdk_session_id.clone();
    harness
        .provider
        .emit(
            &sdk_id,
            json!({"id":"user","type":"user.message","data":{"content":"work"}}),
        )
        .await
        .expect("emit user message");
    harness
        .provider
        .emit(
            &sdk_id,
            json!({
                "id":"assistant",
                "type":"assistant.message",
                "data":{"content":"x".repeat(6_000)}
            }),
        )
        .await
        .expect("emit assistant message");
    let _interaction = harness
        .provider
        .request_interaction(
            &sdk_id,
            InteractionRequest {
                id: "plan".to_owned(),
                session_id: String::new(),
                kind: InteractionKind::ExitPlanMode,
                title: "Plan ready".to_owned(),
                message: "Implement storage, delivery, and UI state.".to_owned(),
                choices: Vec::new(),
                allow_freeform: false,
                details: json!({"plan":"bounded"}),
            },
        )
        .await
        .expect("request plan approval");
    wait_for(&child, |snapshot| {
        snapshot.transcript.len() == 2 && !snapshot.pending_interactions.is_empty()
    })
    .await;

    let result = harness
        .manager
        .get_session_for_coordination(harness.parent.id(), child.id())
        .await
        .expect("authorized inspection");

    assert_eq!(result.relationship, "ancestor");
    assert_eq!(result.status, "waiting");
    assert_eq!(result.pending_plan_summary.as_deref(), Some("bounded"));
    assert_eq!(result.transcript_tail.len(), 2);
    assert!(result.latest_assistant_result.unwrap().chars().count() <= 4_000);
    assert!(result.changes.paths.len() <= 20);
    assert!(
        harness
            .manager
            .get_session_for_coordination(child.id(), harness.parent.id())
            .await
            .is_ok()
    );
    assert!(
        harness
            .manager
            .get_session_for_coordination(child.id(), child.id())
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn messages_are_idempotent_and_respect_immediate_and_queued_delivery() {
    let (harness, _) = harness(false).await;
    let queued_child = harness
        .manager
        .create_session(child_request(
            harness.directory.path(),
            harness.parent.id(),
            "Queued child",
            "queued-child",
        ))
        .await
        .expect("create queued child");
    let immediate_child = harness
        .manager
        .create_session(child_request(
            harness.directory.path(),
            harness.parent.id(),
            "Immediate child",
            "immediate-child",
        ))
        .await
        .expect("create immediate child");

    let queued = harness
        .manager
        .send_session_message(
            harness.parent.id(),
            "send-queued",
            queued_child.id(),
            "Run after the current turn.",
            SessionMessageDelivery::Queued,
        )
        .await
        .expect("queue message");
    harness
        .manager
        .send_session_message(
            harness.parent.id(),
            "send-blocker",
            immediate_child.id(),
            "This queued message stays behind the steering message.",
            SessionMessageDelivery::Queued,
        )
        .await
        .expect("queue blocker");
    let immediate = harness
        .manager
        .send_session_message(
            harness.parent.id(),
            "send-immediate",
            immediate_child.id(),
            "Change direction now.",
            SessionMessageDelivery::Immediate,
        )
        .await
        .expect("steer message");
    let retry = harness
        .manager
        .send_session_message(
            harness.parent.id(),
            "send-immediate",
            immediate_child.id(),
            "Changed retry text must not duplicate delivery.",
            SessionMessageDelivery::Immediate,
        )
        .await
        .expect("retry message");

    assert_eq!(immediate.message_id, retry.message_id);
    assert_eq!(queued.state, "pending");
    tokio::time::sleep(Duration::from_millis(50)).await;
    let queued_sdk = queued_child.snapshot().metadata.sdk_session_id.clone();
    let immediate_sdk = immediate_child.snapshot().metadata.sdk_session_id.clone();
    assert!(
        harness
            .provider
            .delivered_queue(&queued_sdk)
            .await
            .is_empty()
    );
    let deliveries = harness.provider.delivered_queue(&immediate_sdk).await;
    assert_eq!(deliveries.len(), 1);
    assert!(deliveries[0].prompt.contains("Change direction now."));
    assert_eq!(
        harness
            .storage
            .list_coordination_items(immediate_child.id(), 100)
            .expect("coordination ledger")
            .len(),
        2
    );
    assert_eq!(
        harness
            .storage
            .queue_view(immediate_child.id())
            .unwrap()
            .pending_count(),
        1
    );
}

#[tokio::test]
async fn dormant_recipient_drains_durable_coordination_when_resumed() {
    let (harness, _) = harness(false).await;
    let child = harness
        .manager
        .create_session(child_request(
            harness.directory.path(),
            harness.parent.id(),
            "Dormant child",
            "dormant-child",
        ))
        .await
        .expect("create child");
    let child_id = child.id().to_owned();
    let sdk_id = child.snapshot().metadata.sdk_session_id.clone();
    harness
        .manager
        .close_session(&child_id)
        .await
        .expect("close child");

    harness
        .manager
        .send_session_message(
            harness.parent.id(),
            "send-dormant",
            &child_id,
            "Deliver after reconnect.",
            SessionMessageDelivery::Queued,
        )
        .await
        .expect("persist dormant message");
    assert!(harness.provider.delivered_queue(&sdk_id).await.is_empty());

    harness
        .manager
        .resume_closed_session(&child_id)
        .await
        .expect("resume child");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let delivered = harness.provider.delivered_queue(&sdk_id).await;
        if delivered.len() == 1 {
            assert!(delivered[0].prompt.contains("Deliver after reconnect."));
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "dormant coordination did not drain on resume"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn child_idle_failure_and_cancel_emit_lifecycle_without_focus_or_selection_changes() {
    let (harness, mut events) = harness(true).await;
    let mut events = events.take().expect("gateway receiver");
    let statuses = [
        ("Idle child", "idle-child", ChildLifecycleStatus::Idle),
        ("Failed child", "failed-child", ChildLifecycleStatus::Failed),
        (
            "Cancelled child",
            "cancelled-child",
            ChildLifecycleStatus::Cancelled,
        ),
    ];
    for (title, tool_call_id, expected) in statuses {
        let child = harness
            .manager
            .create_session(child_request(
                harness.directory.path(),
                harness.parent.id(),
                title,
                tool_call_id,
            ))
            .await
            .expect("create child");
        let sdk_id = child.snapshot().metadata.sdk_session_id.clone();
        let event = match expected {
            ChildLifecycleStatus::Idle => {
                json!({"id":format!("{tool_call_id}-idle"),"type":"session.idle","data":{}})
            }
            ChildLifecycleStatus::Failed => {
                json!({"id":format!("{tool_call_id}-failed"),"type":"session.error","data":{"message":"failed"}})
            }
            ChildLifecycleStatus::Cancelled => {
                json!({"id":format!("{tool_call_id}-cancelled"),"type":"session.idle","data":{"aborted":true}})
            }
        };
        harness
            .provider
            .emit(&sdk_id, event)
            .await
            .expect("emit lifecycle");
        let received = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("lifecycle timeout")
            .expect("lifecycle event");
        assert!(matches!(
            received,
            HostGatewayEvent::ChildLifecycle(event)
                if event.child_session_id == child.id() && event.status == expected
        ));
    }
    assert_eq!(
        harness
            .manager
            .selected_session()
            .expect("selection")
            .as_deref(),
        None
    );
}

#[tokio::test]
async fn busy_parent_receives_child_notifications_in_durable_order() {
    let (harness, _) = harness(false).await;
    let first = harness
        .manager
        .create_session(child_request(
            harness.directory.path(),
            harness.parent.id(),
            "First child",
            "first-child",
        ))
        .await
        .expect("create first child");
    let second = harness
        .manager
        .create_session(child_request(
            harness.directory.path(),
            harness.parent.id(),
            "Second child",
            "second-child",
        ))
        .await
        .expect("create second child");
    for child in [&first, &second] {
        harness
            .manager
            .set_host_tool_notify_on_idle(child.id(), Some("once"))
            .unwrap();
        harness
            .manager
            .complete_host_tool_launch(child.id())
            .unwrap();
    }

    harness
        .manager
        .handle_child_lifecycle(&ChildLifecycleEvent {
            child_session_id: first.id().to_owned(),
            title: "First child".to_owned(),
            status: ChildLifecycleStatus::Idle,
        })
        .await
        .expect("first notification");
    harness
        .manager
        .handle_child_lifecycle(&ChildLifecycleEvent {
            child_session_id: second.id().to_owned(),
            title: "Second child".to_owned(),
            status: ChildLifecycleStatus::Failed,
        })
        .await
        .expect("second notification");

    let parent_sdk = harness.parent.snapshot().metadata.sdk_session_id.clone();
    assert!(
        harness
            .provider
            .delivered_queue(&parent_sdk)
            .await
            .is_empty()
    );
    let queued = harness.storage.queue_view(harness.parent.id()).unwrap();
    assert_eq!(queued.pending_count(), 2);
    assert!(queued.items[0].prompt.contains("First child"));
    assert!(queued.items[1].prompt.contains("Second child"));

    harness
        .provider
        .emit(
            &parent_sdk,
            json!({"id":"parent-idle-1","type":"session.idle","data":{}}),
        )
        .await
        .expect("first parent idle");
    wait_for(&harness.parent, |snapshot| {
        snapshot
            .queue
            .items
            .iter()
            .any(|item| item.state == app_model::QueueItemState::Dispatched)
    })
    .await;
    harness
        .provider
        .emit(
            &parent_sdk,
            json!({"id":"parent-idle-2","type":"session.idle","data":{}}),
        )
        .await
        .expect("second parent idle");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let delivered = harness.provider.delivered_queue(&parent_sdk).await;
        if delivered.len() == 2 {
            assert!(delivered[0].prompt.contains("First child"));
            assert!(delivered[1].prompt.contains("Second child"));
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "notifications did not drain in order"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn root_metadata(path: &Path) -> SessionMetadata {
    SessionMetadata {
        id: "parent".to_owned(),
        sdk_session_id: "sdk-parent".to_owned(),
        project_path: path.to_string_lossy().into_owned(),
        repository_root: Some(path.to_string_lossy().into_owned()),
        title: "Parent".to_owned(),
        title_source: TitleSource::Manual,
        kind: SessionKind::Project,
        parent_session_id: None,
        launch_origin: SessionLaunchOrigin::User,
        host_tool_call_id: None,
        model: None,
        mode: Some("autopilot".to_owned()),
        base_ref: Some("main".to_owned()),
        created_at: "1".to_owned(),
        updated_at: "1".to_owned(),
    }
}

#[tokio::test]
async fn completion_notification_is_exactly_once_across_storage_reopen() {
    let directory = tempfile::tempdir().expect("temporary project");
    let database = directory.path().join("coordination.db");
    let event = ChildLifecycleEvent {
        child_session_id: "child".to_owned(),
        title: "Child".to_owned(),
        status: ChildLifecycleStatus::Failed,
    };
    {
        let storage = Arc::new(Storage::open(&database).expect("storage"));
        let parent = root_metadata(directory.path());
        let mut child = root_metadata(directory.path());
        child.id = "child".to_owned();
        child.sdk_session_id = "sdk-child".to_owned();
        child.title = "Child".to_owned();
        child.parent_session_id = Some(parent.id.clone());
        child.launch_origin = SessionLaunchOrigin::AgentTool;
        child.host_tool_call_id = Some("create-child".to_owned());
        storage.upsert_project(&project(directory.path())).unwrap();
        storage.upsert_session(&parent).unwrap();
        storage.upsert_session(&child).unwrap();
        storage
            .set_host_tool_notify_on_idle(&child.id, Some("once"))
            .unwrap();
        storage.complete_host_tool_launch(&child.id).unwrap();
        let manager = SessionManager::new(
            Arc::new(FakeProvider::default()),
            storage.clone(),
            Arc::new(MemoryDiagnostics::default()),
        );
        assert!(
            manager
                .handle_child_lifecycle(&event)
                .await
                .expect("first notification")
                .is_some()
        );
    }

    let storage = Arc::new(Storage::open(&database).expect("reopen storage"));
    let manager = SessionManager::new(
        Arc::new(FakeProvider::default()),
        storage.clone(),
        Arc::new(MemoryDiagnostics::default()),
    );
    assert!(
        manager
            .handle_child_lifecycle(&event)
            .await
            .expect("retry notification")
            .is_none()
    );
    let records = storage
        .list_coordination_items("parent", 100)
        .expect("coordination records");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].kind, CoordinationKind::ChildCompletion);
    assert_eq!(storage.queue_view("parent").unwrap().items.len(), 1);
}
