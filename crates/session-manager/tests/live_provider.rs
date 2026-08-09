use std::sync::Arc;
use std::time::Duration;

use app_model::{SessionKind, SessionStatus, TranscriptRole, TranscriptState};
use copilot_provider::CopilotProvider;
use diagnostics::MemoryDiagnostics;
use session_manager::{CreateSessionRequest, SessionManager};
use storage::Storage;
use tempfile::tempdir;

#[tokio::test]
#[ignore = "uses the real Copilot SDK, account, network, and model quota"]
async fn real_provider_streams_a_complete_transcript() {
    let project = tempdir().unwrap();
    let diagnostics = Arc::new(MemoryDiagnostics::default());
    let provider = Arc::new(CopilotProvider::new(project.path(), diagnostics.clone()));
    let storage = Arc::new(Storage::open_in_memory().unwrap());
    let manager = SessionManager::new(provider, storage, diagnostics);
    manager.start().await.unwrap();
    let session = manager
        .create_session(CreateSessionRequest {
            project_path: project.path().to_owned(),
            repository_root: None,
            title: "Live transcript smoke".to_owned(),
            kind: SessionKind::Project,
            model: None,
            mode: Some("interactive".to_owned()),
            reasoning_effort: Some("low".to_owned()),
            context_tier: None,
            base_ref: None,
        })
        .await
        .unwrap();
    let mut snapshots = session.subscribe();

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
