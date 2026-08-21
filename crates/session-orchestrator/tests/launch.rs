use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use app_model::{PromptAttachment, SessionKind, SessionLocation, TitleSource};
use diagnostics::MemoryDiagnostics;
use git_service::GitService;
use session_manager::{SessionManager, SessionRoots};
use session_orchestrator::{
    LaunchOrigin, LaunchProgress, LaunchRequest, LaunchStage, LaunchTitle, SessionOrchestrator,
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
    }
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
