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
        agent: None,
        reasoning_effort: Some("low".to_owned()),
        context_tier: None,
        base_ref: None,
        unattended: false,
    }
}
