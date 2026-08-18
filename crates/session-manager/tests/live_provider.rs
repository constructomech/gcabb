use std::sync::Arc;
use std::time::Duration;

use app_model::{SessionKind, SessionStatus, TitleSource, TranscriptRole, TranscriptState};
use copilot_provider::CopilotProviderFactory;
use diagnostics::MemoryDiagnostics;
use session_manager::{CreateSessionRequest, SessionManager};
use storage::Storage;
use tempfile::tempdir;

#[tokio::test]
#[ignore = "uses the real Copilot SDK, account, network, and model quota"]
async fn isolated_real_provider_survives_sibling_shutdown() {
    let project = tempdir().unwrap();
    let diagnostics = Arc::new(MemoryDiagnostics::default());
    let provider_factory = CopilotProviderFactory::new(project.path(), diagnostics.clone());
    let storage = Arc::new(Storage::open_in_memory().unwrap());
    let manager = SessionManager::new(provider_factory, storage, diagnostics.clone());
    manager.start().await.unwrap();
    let first = manager
        .create_session(request(project.path(), "First isolated runtime"))
        .await
        .unwrap();
    let session = manager
        .create_session(request(project.path(), "Second isolated runtime"))
        .await
        .unwrap();
    let mut snapshots = session.subscribe();
    let process_ids = diagnostics
        .events()
        .into_iter()
        .filter(|event| event.operation == "runtime_start")
        .filter_map(|event| event.details["processId"].as_u64())
        .collect::<Vec<_>>();
    assert_eq!(process_ids.len(), 2);
    assert_ne!(process_ids[0], process_ids[1]);

    manager.close_session(first.id()).await.unwrap();

    session
        .send("Reply with exactly: gcabb-phase-2-ok")
        .await
        .unwrap();
    tokio::time::timeout(
        Duration::from_secs(90),
        snapshots.wait_for(|snapshot| {
            snapshot.status == SessionStatus::Idle
                && snapshot.transcript.iter().any(|message| {
                    message.role == TranscriptRole::Assistant
                        && message.state == TranscriptState::Complete
                        && message.content.contains("gcabb-phase-2-ok")
                })
        }),
    )
    .await
    .unwrap()
    .unwrap();

    session.disconnect().await.unwrap();
    manager.stop().await.unwrap();
}

fn request(project: &std::path::Path, title: &str) -> CreateSessionRequest {
    CreateSessionRequest {
        project_path: project.to_owned(),
        repository_root: None,
        title: title.to_owned(),
        title_source: TitleSource::Manual,
        kind: SessionKind::Project,
        model: None,
        mode: Some("interactive".to_owned()),
        reasoning_effort: Some("low".to_owned()),
        context_tier: None,
        base_ref: None,
    }
}

/// Hosting the session filesystem has to hold up against the real runtime:
/// the agent's SQL must reach GCABB's database, and an edit made there must be
/// what the runtime reads back.
#[tokio::test]
#[ignore = "uses the real Copilot SDK, account, network, and model quota"]
async fn a_hosted_session_shares_the_agents_task_list() {
    use copilot_provider::AgentProvider as _;

    let project = tempdir().unwrap();
    let state = tempdir().unwrap();
    let diagnostics = Arc::new(MemoryDiagnostics::default());
    let provider = Arc::new(
        copilot_provider::CopilotProvider::new(project.path(), diagnostics.clone())
            .hosting_session_state(state.path()),
    );
    provider.start().await.unwrap();
    let session = provider
        .create_session(copilot_provider::SessionRequest {
            working_directory: project.path().to_owned(),
            auto_approve_tools: true,
            ..copilot_provider::SessionRequest::default()
        })
        .await
        .unwrap();
    let sdk_session_id = session.sdk_session_id.clone();

    provider
        .send(
            &sdk_session_id,
            "Use the sql tool to run exactly this query, then reply done: \
             INSERT INTO todos (id, title, status) \
             VALUES ('live-row', 'Agent authored', 'pending')",
            &[],
        )
        .await
        .unwrap();
    let plan = wait_for(Duration::from_mins(2), || async {
        provider
            .agent_plan(&sdk_session_id)
            .await
            .ok()
            .filter(|plan| !plan.is_empty())
    })
    .await
    .expect("the agent's row reaches the hosted database");
    assert!(plan.todos.iter().any(|todo| todo.id == "live-row"));

    // The host edits what the agent wrote.
    assert!(
        provider
            .set_agent_todo_status(&sdk_session_id, "live-row", "done")
            .await
            .unwrap()
    );
    let edited = provider.agent_plan(&sdk_session_id).await.unwrap();
    assert_eq!(
        edited
            .todos
            .iter()
            .find(|todo| todo.id == "live-row")
            .map(|todo| todo.status),
        Some(app_model::AgentTodoStatus::Done)
    );

    // And the host can add a todo of its own.
    provider
        .upsert_agent_todo(
            &sdk_session_id,
            &app_model::AgentTodo {
                id: "host-row".to_owned(),
                title: "Host authored".to_owned(),
                description: Some("Injected by GCABB".to_owned()),
                status: app_model::AgentTodoStatus::Pending,
                depends_on: Vec::new(),
            },
        )
        .await
        .unwrap();
    let combined = provider.agent_plan(&sdk_session_id).await.unwrap();
    assert_eq!(combined.total(), 2);

    provider.disconnect(&sdk_session_id).await.unwrap();
    provider.stop().await.unwrap();
}

async fn wait_for<T, F, Fut>(timeout: Duration, mut attempt: F) -> Option<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(value) = attempt().await {
            return Some(value);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}
