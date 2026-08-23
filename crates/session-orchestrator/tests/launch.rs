use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use app_model::{ProjectMetadata, PromptAttachment, SessionKind, SessionLocation, TitleSource};
use diagnostics::MemoryDiagnostics;
use git_service::GitService;
use session_manager::{SessionManager, SessionRoots};
use session_orchestrator::{
    ForkRequest, LaunchOrigin, LaunchProgress, LaunchRequest, LaunchResult, LaunchStage,
    LaunchTitle, SessionOrchestrator,
};
use storage::Storage;
use test_harness::FakeProviderFactory;

fn git(directory: &Path, arguments: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(arguments)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {arguments:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn repository() -> (tempfile::TempDir, PathBuf) {
    let directory = tempfile::tempdir().expect("temporary repository");
    let repository = directory.path().join("main");
    std::fs::create_dir_all(&repository).expect("create repository");
    git(&repository, &["init", "--initial-branch=main"]);
    git(&repository, &["config", "user.email", "test@example.com"]);
    git(&repository, &["config", "user.name", "Test"]);
    std::fs::write(repository.join("README.md"), "base\n").expect("write fixture");
    git(&repository, &["add", "."]);
    git(&repository, &["commit", "-m", "base"]);
    (directory, repository)
}

fn harness(
    factory: FakeProviderFactory,
    worktrees_root: PathBuf,
) -> (Arc<SessionManager>, SessionOrchestrator) {
    let roots = SessionRoots {
        worktrees: Some(worktrees_root),
        managed_worktrees: Vec::new(),
        attachments: None,
        runtime_state: None,
    };
    let manager = Arc::new(
        SessionManager::new(
            factory,
            Arc::new(Storage::open_in_memory().expect("storage")),
            Arc::new(MemoryDiagnostics::default()),
        )
        .with_session_roots(roots.clone()),
    );
    let orchestrator = SessionOrchestrator::new(manager.clone(), roots);
    (manager, orchestrator)
}

fn harness_with_storage(
    factory: FakeProviderFactory,
    worktrees_root: PathBuf,
) -> (Arc<SessionManager>, SessionOrchestrator, Arc<Storage>) {
    let roots = SessionRoots {
        worktrees: Some(worktrees_root),
        managed_worktrees: Vec::new(),
        attachments: None,
        runtime_state: None,
    };
    let storage = Arc::new(Storage::open_in_memory().expect("storage"));
    let manager = Arc::new(
        SessionManager::new(
            factory,
            storage.clone(),
            Arc::new(MemoryDiagnostics::default()),
        )
        .with_session_roots(roots.clone()),
    );
    let orchestrator = SessionOrchestrator::new(manager.clone(), roots);
    (manager, orchestrator, storage)
}

fn project_request(
    repository: &Path,
    worktrees_root: &Path,
    origin: LaunchOrigin,
) -> LaunchRequest {
    LaunchRequest {
        project_path: repository.to_owned(),
        repository_root: Some(repository.to_owned()),
        worktrees_root: worktrees_root.to_owned(),
        kind: SessionKind::Project,
        location: SessionLocation::NewWorktree,
        prompt: "Implement deterministic launch behavior".to_owned(),
        attachments: vec![PromptAttachment::File {
            path: repository.join("README.md").to_string_lossy().into_owned(),
            display_name: "README.md".to_owned(),
        }],
        model: Some("model-1".to_owned()),
        agent: None,
        mode: "autopilot".to_owned(),
        reasoning_effort: Some("high".to_owned()),
        context_tier: Some("long_context".to_owned()),
        base_ref: Some("main".to_owned()),
        title: LaunchTitle::Automatic,
        origin,
        parent_session_id: None,
        host_tool_call_id: None,
        notify_on_idle: None,
    }
}

fn register_project(manager: &SessionManager, repository: &Path) {
    manager
        .register_project(&ProjectMetadata {
            id: "project".to_owned(),
            path: repository.to_string_lossy().into_owned(),
            name: "Project".to_owned(),
            default_branch: Some("main".to_owned()),
            last_opened_at: "now".to_owned(),
        })
        .expect("register project");
}

async fn source_with_history_and_changes(
    manager: &SessionManager,
    orchestrator: &SessionOrchestrator,
    factory: &FakeProviderFactory,
    repository: &Path,
    worktrees: &Path,
) -> LaunchResult {
    register_project(manager, repository);
    let source = orchestrator
        .launch(
            project_request(repository, worktrees, LaunchOrigin::UserActivation),
            |_| {},
        )
        .await
        .expect("source launch");
    let source_sdk_id = source.handle.snapshot().metadata.sdk_session_id.clone();
    let source_provider = factory.providers().remove(0);
    for event in [
        serde_json::json!({
            "id": "source-event-1",
            "type": "user.message",
            "data": {"content": "first"}
        }),
        serde_json::json!({
            "id": "source-event-2",
            "type": "assistant.message",
            "data": {"content": "second"}
        }),
    ] {
        source_provider
            .emit(&source_sdk_id, event)
            .await
            .expect("source event");
    }
    let source_path = &source.project_path;
    std::fs::write(source_path.join(".gitignore"), "ignored.secret\n").expect("ignore");
    std::fs::write(source_path.join("renamed.txt"), "rename me\n").expect("rename fixture");
    std::fs::write(source_path.join("deleted.txt"), "delete me\n").expect("delete fixture");
    git(
        source_path,
        &["add", ".gitignore", "renamed.txt", "deleted.txt"],
    );
    git(source_path, &["commit", "-m", "fork fixtures"]);
    std::fs::write(source_path.join("README.md"), "staged\n").expect("staged");
    git(source_path, &["add", "README.md"]);
    std::fs::write(source_path.join("README.md"), "unstaged after staged\n").expect("unstaged");
    git(source_path, &["mv", "renamed.txt", "moved.txt"]);
    std::fs::remove_file(source_path.join("deleted.txt")).expect("delete tracked");
    std::fs::write(source_path.join("scratch.bin"), [0, 1, 2, 255]).expect("untracked");
    std::fs::write(source_path.join("ignored.secret"), "do not copy\n").expect("ignored");
    source
}

#[tokio::test]
async fn worktree_launch_uses_generated_title_and_delivers_the_kickoff() {
    let (_guard, repository) = repository();
    let worktrees = tempfile::tempdir().expect("worktrees");
    let factory = FakeProviderFactory::default();
    factory.set_generated_title("Generated launch title");
    let (_manager, orchestrator) = harness(factory.clone(), worktrees.path().to_owned());
    let mut progress = Vec::new();
    let request = project_request(&repository, worktrees.path(), LaunchOrigin::UserActivation);
    let expected_attachments = request.attachments.clone();

    let result = orchestrator
        .launch(request, |update| progress.push(update))
        .await
        .expect("launch");

    assert_eq!(result.title, "Generated launch title");
    assert_eq!(result.title_source, TitleSource::Generated);
    assert_eq!(
        result.branch.as_deref(),
        Some("gcabb/generated-launch-title")
    );
    assert_eq!(
        progress,
        vec![
            LaunchProgress::CreatingWorktree,
            LaunchProgress::WorktreeReady(result.project_path.clone())
        ]
    );
    assert!(result.project_path.join("README.md").exists());
    let snapshot = result.handle.snapshot();
    assert_eq!(snapshot.metadata.model.as_deref(), Some("model-1"));
    assert_eq!(snapshot.metadata.mode.as_deref(), Some("autopilot"));
    assert_eq!(snapshot.metadata.base_ref.as_deref(), Some("main"));
    assert_eq!(snapshot.controls.reasoning_effort.as_deref(), Some("high"));
    assert_eq!(
        snapshot.controls.context_tier.as_deref(),
        Some("long_context")
    );
    let providers = factory.providers();
    assert_eq!(
        providers[0].sent_prompts().await,
        vec!["Implement deterministic launch behavior"]
    );
    assert_eq!(
        providers[0].sent_attachments().await,
        vec![expected_attachments]
    );
}

#[tokio::test]
async fn local_and_chat_launches_run_in_place_with_fallback_titles() {
    let directory = tempfile::tempdir().expect("project");
    let worktrees = tempfile::tempdir().expect("worktrees");
    let factory = FakeProviderFactory::default();
    factory.fail_title_generation(true);
    let (_manager, orchestrator) = harness(factory, worktrees.path().to_owned());

    let mut local = project_request(directory.path(), worktrees.path(), LaunchOrigin::Headless);
    local.location = SessionLocation::LocalRepository;
    local.repository_root = Some(directory.path().to_owned());
    local.prompt = "Help".to_owned();
    local.attachments.clear();
    let local_result = orchestrator
        .launch(local, |_| {})
        .await
        .expect("local launch");
    assert_eq!(local_result.project_path, directory.path());
    assert_eq!(local_result.title, "Help");
    assert_eq!(local_result.title_source, TitleSource::Fallback);
    assert!(local_result.branch.is_none());

    let mut chat = project_request(directory.path(), worktrees.path(), LaunchOrigin::Headless);
    chat.kind = SessionKind::Chat;
    chat.repository_root = None;
    chat.prompt.clear();
    chat.attachments.clear();
    let chat_result = orchestrator
        .launch(chat, |_| {})
        .await
        .expect("chat launch");
    assert_eq!(chat_result.project_path, directory.path());
    assert_eq!(chat_result.title, "New session");
    assert_eq!(chat_result.title_source, TitleSource::Fallback);
    assert!(chat_result.branch.is_none());
}

#[tokio::test]
async fn headless_launch_preserves_selection_and_user_launch_activates() {
    let directory = tempfile::tempdir().expect("project");
    let worktrees = tempfile::tempdir().expect("worktrees");
    let factory = FakeProviderFactory::default();
    let (manager, orchestrator) = harness(factory, worktrees.path().to_owned());

    let mut first = project_request(
        directory.path(),
        worktrees.path(),
        LaunchOrigin::UserActivation,
    );
    first.location = SessionLocation::LocalRepository;
    first.repository_root = Some(directory.path().to_owned());
    first.title = LaunchTitle::Provided {
        title: "Visible session".to_owned(),
        source: TitleSource::Manual,
    };
    let visible = orchestrator
        .launch(first, |_| {})
        .await
        .expect("visible launch");
    assert_eq!(
        manager.selected_session().expect("selection").as_deref(),
        Some(visible.handle.id())
    );

    let mut second = project_request(directory.path(), worktrees.path(), LaunchOrigin::Headless);
    second.location = SessionLocation::LocalRepository;
    second.repository_root = Some(directory.path().to_owned());
    second.title = LaunchTitle::Provided {
        title: "Background session".to_owned(),
        source: TitleSource::Manual,
    };
    let background = orchestrator
        .launch(second, |_| {})
        .await
        .expect("headless launch");

    assert_ne!(visible.handle.id(), background.handle.id());
    assert_eq!(
        manager.selected_session().expect("selection").as_deref(),
        Some(visible.handle.id())
    );
}

#[tokio::test]
async fn agent_child_launch_persists_ownership_and_keeps_parent_selected() {
    let (_guard, repository) = repository();
    let worktrees = tempfile::tempdir().expect("worktrees");
    let factory = FakeProviderFactory::default();
    let (manager, orchestrator, storage) =
        harness_with_storage(factory.clone(), worktrees.path().to_owned());
    let mut parent_request =
        project_request(&repository, worktrees.path(), LaunchOrigin::UserActivation);
    parent_request.title = LaunchTitle::Provided {
        title: "Parent".to_owned(),
        source: TitleSource::Manual,
    };
    let parent = orchestrator
        .launch(parent_request, |_| {})
        .await
        .expect("parent launch");

    let mut child_request = project_request(&repository, worktrees.path(), LaunchOrigin::Headless);
    child_request.title = LaunchTitle::Provided {
        title: "Agent child".to_owned(),
        source: TitleSource::Manual,
    };
    child_request.parent_session_id = Some(parent.handle.id().to_owned());
    child_request.host_tool_call_id = Some("tool-call-1".to_owned());
    child_request.notify_on_idle = Some("once".to_owned());
    let child = orchestrator
        .launch(child_request, |_| {})
        .await
        .expect("child launch");

    assert_ne!(child.handle.id(), parent.handle.id());
    assert_ne!(child.project_path, parent.project_path);
    assert_eq!(factory.providers().len(), 2);
    assert_eq!(
        manager.selected_session().unwrap().as_deref(),
        Some(parent.handle.id())
    );
    let metadata = storage
        .session_metadata(child.handle.id())
        .unwrap()
        .unwrap();
    assert_eq!(
        metadata.parent_session_id.as_deref(),
        Some(parent.handle.id())
    );
    assert_eq!(
        metadata.launch_origin,
        app_model::SessionLaunchOrigin::AgentTool
    );
    assert_eq!(metadata.host_tool_call_id.as_deref(), Some("tool-call-1"));
    assert_eq!(
        storage.host_tool_notify_on_idle(child.handle.id()).unwrap(),
        Some("once".to_owned())
    );
    assert_eq!(
        storage
            .session_for_tool_call(parent.handle.id(), "tool-call-1")
            .unwrap()
            .and_then(|session| session.metadata.map(|metadata| metadata.id)),
        Some(child.handle.id().to_owned())
    );
    assert!(
        storage
            .session_for_tool_call(parent.handle.id(), "tool-call-1")
            .unwrap()
            .unwrap()
            .launch_completed
    );
}

#[tokio::test]
async fn native_fork_preserves_history_filesystem_provenance_and_retry_identity() {
    let (_guard, repository) = repository();
    let worktrees = tempfile::tempdir().expect("worktrees");
    let factory = FakeProviderFactory::default();
    let (manager, orchestrator, storage) =
        harness_with_storage(factory.clone(), worktrees.path().to_owned());
    let source = source_with_history_and_changes(
        &manager,
        &orchestrator,
        &factory,
        &repository,
        worktrees.path(),
    )
    .await;
    let source_path = &source.project_path;
    let source_status = GitService::new(source_path).changes("main", "before".to_owned());

    let request = ForkRequest {
        source_session_id: source.handle.id().to_owned(),
        worktrees_root: worktrees.path().to_owned(),
        to_event_id: Some("source-event-2".to_owned()),
        name: Some("Bounded fork".to_owned()),
        kickoff_prompt: None,
        origin: LaunchOrigin::Headless,
        fork_tool_call_id: Some("fork-tool-1".to_owned()),
    };
    let fork = orchestrator
        .fork(request.clone(), |_| {})
        .await
        .expect("fork");

    assert_eq!(
        manager.selected_session().unwrap().as_deref(),
        Some(source.handle.id()),
        "a headless fork changed selection"
    );
    let metadata = storage
        .session_metadata(fork.handle.id())
        .unwrap()
        .expect("fork metadata");
    assert_eq!(
        metadata.forked_from_session_id.as_deref(),
        Some(source.handle.id())
    );
    assert_eq!(
        metadata.forked_at_event_id.as_deref(),
        Some("source-event-2")
    );
    assert!(metadata.parent_session_id.is_none());
    assert_eq!(
        storage.event_ids(fork.handle.id()).unwrap(),
        std::collections::HashSet::from(["source-event-1".to_owned()])
    );
    assert_eq!(
        std::fs::read(fork.project_path.join("README.md")).unwrap(),
        b"unstaged after staged\n"
    );
    assert!(fork.project_path.join("moved.txt").is_file());
    assert!(!fork.project_path.join("renamed.txt").exists());
    assert!(!fork.project_path.join("deleted.txt").exists());
    assert_eq!(
        std::fs::read(fork.project_path.join("scratch.bin")).unwrap(),
        [0, 1, 2, 255]
    );
    assert!(!fork.project_path.join("ignored.secret").exists());
    assert_eq!(
        GitService::new(source_path)
            .changes("main", "after".to_owned())
            .files,
        source_status.files,
        "forking changed the source checkout"
    );

    let providers_before_retry = factory.providers().len();
    let retry = orchestrator
        .fork(request.clone(), |_| {})
        .await
        .expect("idempotent retry");
    assert_eq!(retry.handle.id(), fork.handle.id());
    assert_eq!(retry.project_path, fork.project_path);
    assert_eq!(factory.providers().len(), providers_before_retry);
    manager
        .close_session(fork.handle.id())
        .await
        .expect("close fork runtime");
    let recovered = orchestrator
        .fork(request, |_| {})
        .await
        .expect("retry after runtime restart");
    assert_eq!(recovered.handle.id(), fork.handle.id());
    assert_eq!(recovered.project_path, fork.project_path);
    assert_eq!(storage.list_sessions().unwrap().len(), 2);
}

#[tokio::test]
async fn full_history_user_fork_selects_the_new_root_session() {
    let (_guard, repository) = repository();
    let worktrees = tempfile::tempdir().expect("worktrees");
    let factory = FakeProviderFactory::default();
    let (manager, orchestrator, storage) =
        harness_with_storage(factory.clone(), worktrees.path().to_owned());
    let source = source_with_history_and_changes(
        &manager,
        &orchestrator,
        &factory,
        &repository,
        worktrees.path(),
    )
    .await;
    let full = orchestrator
        .fork(
            ForkRequest {
                source_session_id: source.handle.id().to_owned(),
                worktrees_root: worktrees.path().to_owned(),
                to_event_id: None,
                name: Some("Full fork".to_owned()),
                kickoff_prompt: None,
                origin: LaunchOrigin::UserActivation,
                fork_tool_call_id: None,
            },
            |_| {},
        )
        .await
        .expect("full fork");
    assert_eq!(
        storage.event_ids(full.handle.id()).unwrap(),
        std::collections::HashSet::from(
            ["source-event-1".to_owned(), "source-event-2".to_owned(),]
        )
    );
    assert_eq!(
        manager.selected_session().unwrap().as_deref(),
        Some(full.handle.id())
    );
    assert!(full.handle.snapshot().metadata.parent_session_id.is_none());
}

#[tokio::test]
async fn invalid_fork_boundary_cleans_the_target_and_leaves_source_unchanged() {
    let (_guard, repository) = repository();
    let worktrees = tempfile::tempdir().expect("worktrees");
    let factory = FakeProviderFactory::default();
    let (manager, orchestrator, storage) =
        harness_with_storage(factory, worktrees.path().to_owned());
    register_project(&manager, &repository);
    let source = orchestrator
        .launch(
            project_request(&repository, worktrees.path(), LaunchOrigin::UserActivation),
            |_| {},
        )
        .await
        .expect("source launch");
    std::fs::write(source.project_path.join("scratch.txt"), "source only\n").expect("source work");
    let source_status = GitService::new(&source.project_path)
        .changes("main", "before".to_owned())
        .files;

    let result = orchestrator
        .fork(
            ForkRequest {
                source_session_id: source.handle.id().to_owned(),
                worktrees_root: worktrees.path().to_owned(),
                to_event_id: Some("foreign-event".to_owned()),
                name: Some("Invalid fork".to_owned()),
                kickoff_prompt: None,
                origin: LaunchOrigin::Headless,
                fork_tool_call_id: None,
            },
            |_| {},
        )
        .await;
    let Err(error) = result else {
        panic!("foreign boundary must fail");
    };
    assert_eq!(error.stage, LaunchStage::Runtime);
    assert!(error.to_string().contains("foreign-event"));
    assert!(error.cleanup.is_empty(), "{:?}", error.cleanup);
    assert_eq!(storage.list_sessions().unwrap().len(), 1);
    assert_eq!(
        GitService::new(&source.project_path)
            .changes("main", "after".to_owned())
            .files,
        source_status
    );
    let namespace = worktrees.path().join("main");
    let remaining = std::fs::read_dir(namespace)
        .map(|entries| {
            entries
                .filter_map(|entry| {
                    let entry = entry.unwrap();
                    entry
                        .file_type()
                        .unwrap()
                        .is_dir()
                        .then(|| entry.file_name())
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    assert_eq!(remaining.len(), 1, "failed fork left paths: {remaining:?}");
}

#[tokio::test]
async fn failed_fork_kickoff_retry_is_explicitly_indeterminate() {
    let (_guard, repository) = repository();
    let worktrees = tempfile::tempdir().expect("worktrees");
    let factory = FakeProviderFactory::default();
    let (manager, orchestrator, storage) =
        harness_with_storage(factory.clone(), worktrees.path().to_owned());
    register_project(&manager, &repository);
    let source = orchestrator
        .launch(
            project_request(&repository, worktrees.path(), LaunchOrigin::UserActivation),
            |_| {},
        )
        .await
        .expect("source launch");
    factory.fail_sends(true);
    let request = ForkRequest {
        source_session_id: source.handle.id().to_owned(),
        worktrees_root: worktrees.path().to_owned(),
        to_event_id: None,
        name: Some("Steered fork".to_owned()),
        kickoff_prompt: Some("Take the alternate approach".to_owned()),
        origin: LaunchOrigin::Headless,
        fork_tool_call_id: Some("fork-with-kickoff".to_owned()),
    };

    let first = orchestrator.fork(request.clone(), |_| {}).await;
    assert!(matches!(
        first,
        Err(ref error) if error.stage == LaunchStage::Kickoff
    ));
    let sessions = storage.list_sessions().unwrap();
    assert_eq!(sessions.len(), 2);
    assert!(
        sessions
            .iter()
            .find(|metadata| metadata.id != source.handle.id())
            .unwrap()
            .fork_kickoff_pending
    );

    let retry = orchestrator.fork(request, |_| {}).await;
    let Err(error) = retry else {
        panic!("retry must not silently drop or duplicate kickoff");
    };
    assert!(error.to_string().contains("indeterminate"), "{error}");
    assert_eq!(storage.list_sessions().unwrap().len(), 2);
}

#[tokio::test]
async fn concurrent_launches_allocate_distinct_worktrees() {
    let (_guard, repository) = repository();
    let worktrees = tempfile::tempdir().expect("worktrees");
    let factory = FakeProviderFactory::default();
    let (_manager, orchestrator) = harness(factory, worktrees.path().to_owned());
    let mut first = project_request(&repository, worktrees.path(), LaunchOrigin::Headless);
    first.title = LaunchTitle::Provided {
        title: "Concurrent child".to_owned(),
        source: TitleSource::Manual,
    };
    let second = first.clone();

    let (first, second) = tokio::join!(
        orchestrator.launch(first, |_| {}),
        orchestrator.launch(second, |_| {})
    );
    let first = first.expect("first launch");
    let second = second.expect("second launch");

    assert_ne!(first.project_path, second.project_path);
    assert_ne!(first.branch, second.branch);
}

#[tokio::test]
async fn failed_agent_child_launch_compensates_session_and_idempotency_record() {
    let (_guard, repository) = repository();
    let worktrees = tempfile::tempdir().expect("worktrees");
    let factory = FakeProviderFactory::default();
    let (_manager, orchestrator, storage) =
        harness_with_storage(factory.clone(), worktrees.path().to_owned());
    let mut parent_request =
        project_request(&repository, worktrees.path(), LaunchOrigin::UserActivation);
    parent_request.title = LaunchTitle::Provided {
        title: "Parent".to_owned(),
        source: TitleSource::Manual,
    };
    let parent = orchestrator
        .launch(parent_request, |_| {})
        .await
        .expect("parent launch");
    factory.fail_sends(true);

    let mut child_request = project_request(&repository, worktrees.path(), LaunchOrigin::Headless);
    child_request.title = LaunchTitle::Provided {
        title: "Failing child".to_owned(),
        source: TitleSource::Manual,
    };
    child_request.parent_session_id = Some(parent.handle.id().to_owned());
    child_request.host_tool_call_id = Some("tool-call-failure".to_owned());
    assert!(orchestrator.launch(child_request, |_| {}).await.is_err());

    let compensated = storage
        .session_for_tool_call(parent.handle.id(), "tool-call-failure")
        .unwrap()
        .expect("failed launch tombstone");
    assert!(compensated.metadata.is_none());
    assert!(!compensated.launch_completed);
    assert_eq!(storage.list_sessions().unwrap().len(), 1);
}

#[tokio::test]
async fn branch_collisions_use_predictable_suffixes() {
    let (_guard, repository) = repository();
    let worktrees = tempfile::tempdir().expect("worktrees");
    let factory = FakeProviderFactory::default();
    factory.set_generated_title("Same title");
    let (_manager, orchestrator) = harness(factory, worktrees.path().to_owned());

    let first = orchestrator
        .launch(
            project_request(&repository, worktrees.path(), LaunchOrigin::Headless),
            |_| {},
        )
        .await
        .expect("first launch");
    let second = orchestrator
        .launch(
            project_request(&repository, worktrees.path(), LaunchOrigin::Headless),
            |_| {},
        )
        .await
        .expect("second launch");

    assert_eq!(first.branch.as_deref(), Some("gcabb/same-title"));
    assert_eq!(second.branch.as_deref(), Some("gcabb/same-title-2"));
    assert_ne!(first.project_path, second.project_path);
}

#[tokio::test]
async fn runtime_failure_removes_the_new_clean_worktree_and_branch() {
    let (_guard, repository) = repository();
    let worktrees = tempfile::tempdir().expect("worktrees");
    let factory = FakeProviderFactory::default();
    factory.set_generated_title("Runtime failure");
    factory.fail_starts(true);
    factory.fail_stops(true);
    let (manager, orchestrator) = harness(factory.clone(), worktrees.path().to_owned());
    let expected_path = worktrees.path().join("main").join("gcabb-runtime-failure");

    let error = orchestrator
        .launch(
            project_request(&repository, worktrees.path(), LaunchOrigin::Headless),
            |_| {},
        )
        .await
        .err()
        .expect("runtime must fail");

    assert_eq!(error.stage, LaunchStage::Runtime);
    assert!(error.cleanup.is_empty(), "{:?}", error.cleanup);
    assert!(error.error.contains("runtime cleanup failed"));
    assert!(!expected_path.exists());
    assert!(!GitService::new(&repository).branch_exists("gcabb/runtime-failure"));
    assert!(manager.sessions().await.is_empty());
    assert!(!factory.providers()[0].is_started());
}

#[tokio::test]
async fn send_failure_removes_runtime_and_clean_worktree_and_surfaces_stop_failure() {
    let (_guard, repository) = repository();
    let worktrees = tempfile::tempdir().expect("worktrees");
    let factory = FakeProviderFactory::default();
    factory.set_generated_title("Send failure");
    factory.fail_sends(true);
    factory.fail_stops(true);
    let (manager, orchestrator) = harness(factory, worktrees.path().to_owned());
    manager
        .set_selected_session(Some("existing-session"))
        .expect("initial selection");
    let expected_path = worktrees.path().join("main").join("gcabb-send-failure");

    let error = orchestrator
        .launch(
            project_request(&repository, worktrees.path(), LaunchOrigin::UserActivation),
            |_| {},
        )
        .await
        .err()
        .expect("send must fail");

    assert_eq!(error.stage, LaunchStage::Kickoff);
    assert!(
        error
            .cleanup
            .iter()
            .any(|failure| failure.operation == "stop failed runtime")
    );
    assert!(!expected_path.exists());
    assert!(!GitService::new(&repository).branch_exists("gcabb/send-failure"));
    assert!(manager.sessions().await.is_empty());
    assert_eq!(
        manager.selected_session().expect("selection").as_deref(),
        Some("existing-session")
    );
}

#[tokio::test]
async fn failed_send_preserves_a_dirty_new_worktree_and_reports_it() {
    let (_guard, repository) = repository();
    let worktrees = tempfile::tempdir().expect("worktrees");
    let factory = FakeProviderFactory::default();
    factory.set_generated_title("Dirty failure");
    factory.fail_sends(true);
    factory.dirty_on_send_failure(true);
    let (manager, orchestrator) = harness(factory, worktrees.path().to_owned());
    let expected_path = worktrees.path().join("main").join("gcabb-dirty-failure");

    let error = orchestrator
        .launch(
            project_request(&repository, worktrees.path(), LaunchOrigin::Headless),
            |_| {},
        )
        .await
        .err()
        .expect("send must fail");

    assert_eq!(error.stage, LaunchStage::Kickoff);
    assert!(
        error
            .cleanup
            .iter()
            .any(|failure| failure.error.contains("has changes and was preserved"))
    );
    assert!(expected_path.join("unsaved-from-failed-send.txt").exists());
    assert!(GitService::new(&repository).branch_exists("gcabb/dirty-failure"));
    assert!(manager.sessions().await.is_empty());
}

/// Two repositories with the same directory name must not share a namespace.
#[tokio::test]
async fn same_named_repositories_get_distinct_worktree_namespaces() {
    let (_first_guard, first_repository) = repository();
    let (_second_guard, second_repository) = repository();
    let worktrees = tempfile::tempdir().expect("worktrees");
    assert_eq!(first_repository.file_name(), second_repository.file_name());

    let factory = FakeProviderFactory::default();
    factory.set_generated_title("Same title");
    let (_manager, orchestrator) = harness(factory, worktrees.path().to_owned());

    let first = orchestrator
        .launch(
            project_request(&first_repository, worktrees.path(), LaunchOrigin::Headless),
            |_| {},
        )
        .await
        .expect("first repository launch");
    let second = orchestrator
        .launch(
            project_request(&second_repository, worktrees.path(), LaunchOrigin::Headless),
            |_| {},
        )
        .await
        .expect("second repository launch");

    assert_ne!(first.project_path, second.project_path);
    assert_eq!(namespace_name(&first.project_path), Some("main".to_owned()));
    assert_eq!(
        namespace_name(&second.project_path),
        Some("main-2".to_owned())
    );
}

/// A stale managed directory must not make an otherwise valid launch fail.
#[tokio::test]
async fn stale_worktree_directories_get_a_predictable_namespace_suffix() {
    let (_guard, repository) = repository();
    let worktrees = tempfile::tempdir().expect("worktrees");
    std::fs::create_dir_all(worktrees.path().join("main").join("gcabb-fix-auth-flow"))
        .expect("stale worktree directory");

    let factory = FakeProviderFactory::default();
    factory.set_generated_title("Fix auth flow");
    let (_manager, orchestrator) = harness(factory, worktrees.path().to_owned());

    let launched = orchestrator
        .launch(
            project_request(&repository, worktrees.path(), LaunchOrigin::Headless),
            |_| {},
        )
        .await
        .expect("launch resolved");

    assert_eq!(
        namespace_name(&launched.project_path),
        Some("main-2".to_owned())
    );
    assert_eq!(launched.branch.as_deref(), Some("gcabb/fix-auth-flow"));
}

fn namespace_name(worktree: &Path) -> Option<String> {
    worktree
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .map(str::to_owned)
}
