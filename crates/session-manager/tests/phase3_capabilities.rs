//! Phase 3 self-hosting capability coverage.
//!
//! The deterministic tests prove the app-owned half of the loop — capability
//! discovery, worktree wiring, changes accuracy, and terminal lifetime —
//! without a model or network. The ignored test drives the same loop against
//! the real Copilot runtime.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use app_model::{CapabilityId, CapabilityStatus, ChangeStage, SessionKind, TerminalState};
use copilot_provider::{AgentProvider, CopilotProvider};
use diagnostics::MemoryDiagnostics;
use session_manager::{CreateSessionRequest, SessionManager};
use storage::Storage;
use tempfile::{TempDir, tempdir};
use test_harness::FakeProvider;

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A git worktree standing in for a session checkout.
fn worktree() -> TempDir {
    let dir = tempdir().expect("tempdir");
    let path = dir.path();
    git(path, &["init", "--initial-branch=main"]);
    git(path, &["config", "user.email", "test@example.com"]);
    git(path, &["config", "user.name", "Test"]);
    fs::write(path.join("README.md"), "base\n").expect("write");
    git(path, &["add", "."]);
    git(path, &["commit", "-m", "base"]);
    dir
}

async fn manager_with(provider: Arc<FakeProvider>) -> SessionManager {
    let storage = Arc::new(Storage::open_in_memory().expect("storage"));
    let diagnostics = Arc::new(MemoryDiagnostics::default());
    let manager = SessionManager::new(provider, storage, diagnostics);
    manager.start().await.expect("manager starts");
    manager
}

fn request(path: &Path) -> CreateSessionRequest {
    CreateSessionRequest {
        project_path: path.to_owned(),
        repository_root: None,
        title: "Phase 3 capability".to_owned(),
        kind: SessionKind::Project,
        model: None,
        mode: Some("interactive".to_owned()),
        reasoning_effort: None,
        base_ref: Some("main".to_owned()),
        context_tier: None,
    }
}

/// Exit criterion: file, search, and terminal tools are proven available in a
/// GCABB-created session rather than assumed.
#[tokio::test]
async fn session_discovers_inherited_tools_and_reports_capabilities() {
    let project = worktree();
    let provider = Arc::new(FakeProvider::default());
    provider.add_tools(&["github-mcp-server-search_code"]).await;
    let manager = manager_with(provider).await;

    let session = manager
        .create_session(request(project.path()))
        .await
        .expect("session created");
    let snapshot = session.snapshot();

    assert!(
        snapshot.tool_catalog.is_discovered(),
        "tool discovery must run at session start"
    );
    for name in ["str_replace_editor", "grep", "glob", "bash"] {
        assert!(
            snapshot.tool_catalog.contains(name),
            "expected inherited tool {name} in catalog"
        );
    }

    let capabilities = &snapshot.capabilities;
    for id in [
        CapabilityId::FileRead,
        CapabilityId::FileWrite,
        CapabilityId::Search,
        CapabilityId::Shell,
        CapabilityId::GithubMcp,
        CapabilityId::Skills,
        CapabilityId::Changes,
    ] {
        assert_eq!(
            capabilities.get(id).map(|capability| capability.status),
            Some(CapabilityStatus::Available),
            "capability {id:?} should be available"
        );
    }
    assert!(capabilities.is_self_hosting_ready());
    assert!(capabilities.blocking().is_empty());
}

/// A runtime that stops registering a tool must surface as actionable state,
/// not as an unexplained model failure.
#[tokio::test]
async fn missing_tools_are_reported_as_blocking_capabilities() {
    let project = worktree();
    let provider = Arc::new(FakeProvider::default());
    provider.omit_tools(&["bash", "str_replace_editor"]).await;
    let manager = manager_with(provider).await;

    let session = manager
        .create_session(request(project.path()))
        .await
        .expect("session created");
    let snapshot = session.snapshot();

    assert_eq!(
        snapshot
            .capabilities
            .get(CapabilityId::Shell)
            .map(|capability| capability.status),
        Some(CapabilityStatus::Unavailable)
    );
    assert_eq!(
        snapshot
            .capabilities
            .get(CapabilityId::FileWrite)
            .map(|capability| capability.status),
        Some(CapabilityStatus::Unavailable)
    );
    // The combined editor tool also provided reading, so removing it must take
    // the read capability with it.
    assert_eq!(
        snapshot
            .capabilities
            .get(CapabilityId::FileRead)
            .map(|capability| capability.status),
        Some(CapabilityStatus::Unavailable)
    );
    assert!(!snapshot.capabilities.is_self_hosting_ready());
    let blocking: Vec<CapabilityId> = snapshot
        .capabilities
        .blocking()
        .into_iter()
        .map(|capability| capability.id)
        .collect();
    assert!(blocking.contains(&CapabilityId::Shell));
    assert!(blocking.contains(&CapabilityId::FileWrite));
    // Searching is unaffected by the omitted tools.
    assert!(!blocking.contains(&CapabilityId::Search));
}

/// Tool discovery failure must not prevent the session from running.
#[tokio::test]
async fn tool_discovery_failure_is_visible_without_failing_the_session() {
    let project = worktree();
    let provider = Arc::new(FakeProvider::default());
    provider.fail_tool_discovery(true);
    let manager = manager_with(provider).await;

    let session = manager
        .create_session(request(project.path()))
        .await
        .expect("session still starts when discovery fails");
    let snapshot = session.snapshot();

    assert!(!snapshot.tool_catalog.is_discovered());
    assert!(
        snapshot
            .tool_catalog
            .error
            .as_ref()
            .is_some_and(|error| error.contains("tool discovery unavailable"))
    );
    assert_eq!(
        snapshot
            .capabilities
            .get(CapabilityId::FileRead)
            .map(|capability| capability.status),
        Some(CapabilityStatus::Unknown)
    );
}

/// Exit criterion: the changes view accurately shows committed, staged, and
/// unstaged changes against the session's recorded base.
#[tokio::test]
async fn changes_view_reports_all_stages_against_the_recorded_base() {
    let project = worktree();
    let path = project.path();
    git(path, &["checkout", "-b", "session-work"]);

    fs::write(path.join("committed.txt"), "committed\n").expect("write");
    git(path, &["add", "committed.txt"]);
    git(path, &["commit", "-m", "committed"]);

    fs::write(path.join("staged.txt"), "staged\n").expect("write");
    git(path, &["add", "staged.txt"]);

    fs::write(path.join("README.md"), "modified\n").expect("write");
    fs::write(path.join("untracked.txt"), "untracked\n").expect("write");

    let provider = Arc::new(FakeProvider::default());
    let manager = manager_with(provider).await;
    let session = manager
        .create_session(request(path))
        .await
        .expect("session created");
    let changes = session.snapshot().changes.clone();

    assert!(
        changes.error.is_none(),
        "changes error: {:?}",
        changes.error
    );
    assert_eq!(changes.base_label.as_deref(), Some("main"));
    assert_eq!(
        changes.file("committed.txt").map(|file| file.stage),
        Some(ChangeStage::Committed)
    );
    assert_eq!(
        changes.file("staged.txt").map(|file| file.stage),
        Some(ChangeStage::Staged)
    );
    assert_eq!(
        changes.file("README.md").map(|file| file.stage),
        Some(ChangeStage::Unstaged)
    );
    assert_eq!(
        changes.file("untracked.txt").map(|file| file.stage),
        Some(ChangeStage::Untracked)
    );
    assert!(
        changes
            .file("README.md")
            .and_then(|file| file.diff.as_deref())
            .is_some_and(|diff| diff.contains("+modified"))
    );
    assert_eq!(
        session
            .snapshot()
            .capabilities
            .get(CapabilityId::Changes)
            .map(|capability| capability.status),
        Some(CapabilityStatus::Available)
    );
}

/// The changes view must refresh after a worktree-mutating tool completes,
/// without polling the filesystem.
#[tokio::test]
async fn changes_refresh_after_a_mutating_tool_completes() {
    let project = worktree();
    let path = project.path().to_owned();
    let provider = Arc::new(FakeProvider::default());
    let manager = manager_with(provider.clone()).await;
    let session = manager
        .create_session(request(&path))
        .await
        .expect("session created");
    let sdk_session_id = session.snapshot().metadata.sdk_session_id.clone();

    assert!(
        session.snapshot().changes.is_empty(),
        "worktree starts clean"
    );

    // The agent writes a file, then its edit tool reports completion.
    fs::write(path.join("new-file.rs"), "fn main() {}\n").expect("write");
    provider
        .emit(
            &sdk_session_id,
            serde_json::json!({
                "id": "edit-start",
                "type": "tool.execution_start",
                "timestamp": "1",
                "data": {
                    "toolCallId": "edit-1",
                    "toolName": "create",
                    "arguments": {"path": "new-file.rs"}
                }
            }),
        )
        .await
        .expect("emit start");
    provider
        .emit(
            &sdk_session_id,
            serde_json::json!({
                "id": "edit-complete",
                "type": "tool.execution_complete",
                "timestamp": "2",
                "data": {"toolCallId": "edit-1", "success": true, "result": {"content": "ok"}}
            }),
        )
        .await
        .expect("emit complete");

    let changes = await_snapshot(&session, |snapshot| {
        snapshot.changes.file("new-file.rs").is_some()
    })
    .await;
    assert_eq!(
        changes.changes.file("new-file.rs").map(|file| file.stage),
        Some(ChangeStage::Untracked)
    );
}

/// Exit criterion: commands stream progress and their terminal is addressable
/// by shell id across the tool calls that share it.
#[tokio::test]
async fn shell_activity_is_tracked_per_shell_across_tool_calls() {
    let project = worktree();
    let provider = Arc::new(FakeProvider::default());
    let manager = manager_with(provider.clone()).await;
    let session = manager
        .create_session(request(project.path()))
        .await
        .expect("session created");
    let sdk_session_id = session.snapshot().metadata.sdk_session_id.clone();

    for raw in [
        serde_json::json!({
            "id": "bash-start",
            "type": "tool.execution_start",
            "timestamp": "1",
            "data": {
                "toolCallId": "bash-1",
                "toolName": "bash",
                "arguments": {"command": "cargo test", "shellId": "shell-1"},
                "shellToolInfo": {
                    "displayCommand": "cargo test",
                    "hasWriteFileRedirection": false,
                    "possiblePaths": []
                }
            }
        }),
        serde_json::json!({
            "id": "bash-partial",
            "type": "tool.execution_partial_result",
            "timestamp": "2",
            "data": {"toolCallId": "bash-1", "partialOutput": "compiling\n"}
        }),
        serde_json::json!({
            "id": "read-start",
            "type": "tool.execution_start",
            "timestamp": "3",
            "data": {
                "toolCallId": "read-1",
                "toolName": "read_bash",
                "arguments": {"shellId": "shell-1"}
            }
        }),
        serde_json::json!({
            "id": "read-partial",
            "type": "tool.execution_partial_result",
            "timestamp": "4",
            "data": {"toolCallId": "read-1", "partialOutput": "test result: ok\n"}
        }),
    ] {
        provider.emit(&sdk_session_id, raw).await.expect("emit");
    }

    let snapshot = await_snapshot(&session, |snapshot| {
        snapshot
            .tool_activity
            .terminal("shell-1")
            .is_some_and(|terminal| terminal.output.contains("test result: ok"))
    })
    .await;

    let terminal = snapshot
        .tool_activity
        .terminal("shell-1")
        .expect("terminal exists");
    assert_eq!(
        snapshot.tool_activity.terminals.len(),
        1,
        "both tool calls address one shell"
    );
    assert_eq!(terminal.output, "compiling\ntest result: ok\n");
    assert_eq!(terminal.state, TerminalState::Running);
    assert_eq!(terminal.tool_call_ids.len(), 2);
}

/// A session directory outside git must degrade to an explained capability
/// rather than an error dialog or panic.
#[tokio::test]
async fn non_git_session_directory_reports_changes_unavailable() {
    let project = tempdir().expect("tempdir");
    let provider = Arc::new(FakeProvider::default());
    let manager = manager_with(provider).await;
    let session = manager
        .create_session(request(project.path()))
        .await
        .expect("session created");
    let snapshot = session.snapshot();

    assert_eq!(
        snapshot
            .capabilities
            .get(CapabilityId::Changes)
            .map(|capability| capability.status),
        Some(CapabilityStatus::Unavailable)
    );
    assert!(!snapshot.capabilities.is_self_hosting_ready());
    // Tool capabilities are independent of git and stay available.
    assert_eq!(
        snapshot
            .capabilities
            .get(CapabilityId::Shell)
            .map(|capability| capability.status),
        Some(CapabilityStatus::Available)
    );
}

/// Regression: the changes base must be the repository's default branch, not
/// the session's own branch. Comparing a worktree against the branch it has
/// checked out reports no changes at all, which is what the UI showed.
#[tokio::test]
async fn changes_base_is_the_default_branch_not_the_session_branch() {
    let project = worktree();
    let path = project.path();
    git(path, &["checkout", "-b", "session-branch"]);
    fs::write(path.join("work.txt"), "session work\n").expect("write");
    git(path, &["add", "work.txt"]);
    git(path, &["commit", "-m", "session work"]);

    let provider = Arc::new(FakeProvider::default());
    let manager = manager_with(provider).await;

    // Base recorded as the session's own branch: nothing is reported.
    let mut same_branch = request(path);
    same_branch.base_ref = Some("session-branch".to_owned());
    let session = manager
        .create_session(same_branch)
        .await
        .expect("session created");
    assert!(
        session.snapshot().changes.is_empty(),
        "a branch compared against itself has no changes"
    );

    // Base recorded as the repository default: the work shows up.
    let session = manager
        .create_session(request(path))
        .await
        .expect("session created");
    let changes = session.snapshot().changes.clone();
    assert_eq!(changes.base_label.as_deref(), Some("main"));
    assert_eq!(changes.branch.as_deref(), Some("session-branch"));
    assert!(
        changes.file("work.txt").is_some(),
        "committed session work must appear against the default branch"
    );
}

/// Exit criterion for worktree sessions: deleting a session must not leave
/// its checkout, its git registration, or its branch behind.
#[tokio::test]
async fn deleting_a_worktree_session_reclaims_the_worktree() {
    let project = worktree();
    let repository = project.path();
    let roots = tempdir().expect("tempdir");
    let session_worktree = roots.path().join("gcabb-session");
    git_service::GitService::new(repository)
        .create_worktree(&session_worktree, "gcabb/session", "main")
        .expect("worktree created");
    assert!(session_worktree.exists());

    let provider = Arc::new(FakeProvider::default());
    let storage = Arc::new(Storage::open_in_memory().expect("storage"));
    let diagnostics = Arc::new(MemoryDiagnostics::default());
    let manager = SessionManager::new(provider, storage, diagnostics);
    manager.start().await.expect("manager starts");

    let mut request = request(&session_worktree);
    request.repository_root = Some(repository.to_string_lossy().into_owned());
    let session = manager.create_session(request).await.expect("session");
    let id = session.id().to_owned();

    let deletion = manager
        .delete_session(&id, Some(roots.path()))
        .await
        .expect("session deleted");

    match deletion.worktree {
        Some(app_worktree @ session_manager::WorktreeOutcome::Removed { .. }) => {
            let session_manager::WorktreeOutcome::Removed { branch_removed, .. } = app_worktree
            else {
                unreachable!()
            };
            assert!(branch_removed, "an unmodified branch should be removed");
        }
        other => panic!("expected the worktree to be removed, got {other:?}"),
    }
    assert!(!session_worktree.exists(), "the checkout is gone");
    // Git must not still list it, or `git worktree add` would refuse the path.
    let listed = std::process::Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .expect("git runs");
    let listed = String::from_utf8_lossy(&listed.stdout);
    assert!(
        !listed.contains("gcabb-session"),
        "the worktree registration is pruned: {listed}"
    );
    assert!(!git_service::GitService::new(repository).branch_exists("gcabb/session"));
}

/// Uncommitted work must never be destroyed by deleting a session.
#[tokio::test]
async fn deleting_a_dirty_worktree_session_preserves_the_work() {
    let project = worktree();
    let repository = project.path();
    let roots = tempdir().expect("tempdir");
    let session_worktree = roots.path().join("gcabb-dirty");
    git_service::GitService::new(repository)
        .create_worktree(&session_worktree, "gcabb/dirty", "main")
        .expect("worktree created");
    fs::write(
        session_worktree.join("unsaved.txt"),
        "work in progress
",
    )
    .expect("write");

    let provider = Arc::new(FakeProvider::default());
    let storage = Arc::new(Storage::open_in_memory().expect("storage"));
    let diagnostics = Arc::new(MemoryDiagnostics::default());
    let manager = SessionManager::new(provider, storage, diagnostics);
    manager.start().await.expect("manager starts");
    let mut request = request(&session_worktree);
    request.repository_root = Some(repository.to_string_lossy().into_owned());
    let session = manager.create_session(request).await.expect("session");
    let id = session.id().to_owned();

    let deletion = manager
        .delete_session(&id, Some(roots.path()))
        .await
        .expect("session deleted");

    assert!(
        matches!(
            deletion.worktree,
            Some(session_manager::WorktreeOutcome::PreservedWithChanges { .. })
        ),
        "got {:?}",
        deletion.worktree
    );
    assert!(session_worktree.join("unsaved.txt").exists());
    // The user is told, so a preserved worktree is not silently orphaned.
    assert!(
        deletion
            .worktree
            .and_then(|outcome| outcome.notice())
            .is_some_and(|notice| notice.contains("uncommitted"))
    );
}

/// A session running in the developer's own checkout must never be removed.
#[tokio::test]
async fn deleting_a_local_repository_session_leaves_the_checkout_alone() {
    let project = worktree();
    let repository = project.path();
    let roots = tempdir().expect("tempdir");

    let provider = Arc::new(FakeProvider::default());
    let storage = Arc::new(Storage::open_in_memory().expect("storage"));
    let diagnostics = Arc::new(MemoryDiagnostics::default());
    let manager = SessionManager::new(provider, storage, diagnostics);
    manager.start().await.expect("manager starts");
    let mut request = request(repository);
    request.repository_root = Some(repository.to_string_lossy().into_owned());
    let session = manager.create_session(request).await.expect("session");
    let id = session.id().to_owned();

    let deletion = manager
        .delete_session(&id, Some(roots.path()))
        .await
        .expect("session deleted");

    assert!(deletion.worktree.is_none(), "nothing to reclaim");
    assert!(repository.join("README.md").exists(), "checkout untouched");
}

/// Poll a session's snapshot until `predicate` holds.
async fn await_snapshot(
    session: &session_manager::SessionHandle,
    predicate: impl Fn(&app_model::SessionSnapshot) -> bool,
) -> Arc<app_model::SessionSnapshot> {
    await_snapshot_for(session, Duration::from_secs(5), false, predicate).await
}

/// Poll a session's snapshot, optionally approving permission requests.
///
/// The real runtime asks before writing files and running commands. Nothing
/// else answers those callbacks in a test, so the session would sit in
/// `Waiting` forever unless they are approved here.
async fn await_snapshot_for(
    session: &session_manager::SessionHandle,
    timeout: Duration,
    approve_interactions: bool,
    predicate: impl Fn(&app_model::SessionSnapshot) -> bool,
) -> Arc<app_model::SessionSnapshot> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let snapshot = session.snapshot();
        if predicate(&snapshot) {
            return snapshot;
        }
        if approve_interactions {
            for pending in &snapshot.pending_interactions {
                let _ = session
                    .respond(&pending.id, app_model::InteractionResponse::Approve)
                    .await;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "snapshot condition was not met within {timeout:?}; status={:?}, \
             pending={}, terminals={}, changed={}",
            snapshot.status,
            snapshot.pending_interactions.len(),
            snapshot.tool_activity.terminals.len(),
            snapshot.changes.files.len()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The full self-hosting loop against the real runtime: inspect the repository,
/// change a file, run a command, and review the resulting diff.
#[tokio::test]
#[ignore = "uses the real Copilot SDK, account, network, and model quota"]
async fn real_provider_completes_the_self_hosting_loop() {
    let project = worktree();
    let diagnostics = Arc::new(MemoryDiagnostics::default());
    let provider = Arc::new(CopilotProvider::new(project.path(), diagnostics.clone()));

    // Capability discovery must work against the real runtime before the loop.
    provider.start().await.expect("provider starts");
    let catalog = provider
        .discover_tools(None)
        .await
        .expect("tools.list succeeds against the real runtime");
    let discovered: Vec<&str> = catalog
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect();

    // Assert on capabilities rather than tool names. The model-facing names
    // returned by `tools.list` differ from the CLI's user-facing aliases —
    // file editing arrives as `str_replace_editor`, not `view`/`create`/`edit`
    // — so name assertions would encode a surface the runtime does not promise.
    let report = app_model::CapabilityReport::from_catalog(&catalog);
    for id in [
        CapabilityId::FileRead,
        CapabilityId::FileWrite,
        CapabilityId::Search,
        CapabilityId::Shell,
    ] {
        assert_eq!(
            report.get(id).map(|capability| capability.status),
            Some(CapabilityStatus::Available),
            "capability {id:?} missing from live runtime; discovered: {discovered:?}"
        );
    }

    let storage = Arc::new(Storage::open_in_memory().expect("storage"));
    let manager = SessionManager::new(provider, storage, diagnostics);
    manager.start().await.expect("manager starts");
    let session = manager
        .create_session(request(project.path()))
        .await
        .expect("session created");

    assert!(
        session.snapshot().capabilities.is_self_hosting_ready(),
        "capabilities: {:?}",
        session.snapshot().capabilities
    );

    session
        .send(
            "Create a file named hello.txt containing exactly the text \
             phase-3-ok, then run `cat hello.txt` to confirm it.",
        )
        .await
        .expect("prompt sent");

    // Wait for the loop's artifacts rather than for `Idle`. Trailing events
    // that arrive after `session.idle` currently move the projected status
    // back to `Running`, so status is not a reliable completion signal; the
    // exit criterion is that the edit, the command, and the diff all landed.
    let snapshot = await_snapshot_for(&session, Duration::from_mins(4), true, |snapshot| {
        snapshot.changes.file("hello.txt").is_some() && !snapshot.tool_activity.terminals.is_empty()
    })
    .await;

    // The edit-command-result-diff loop completed inside GCABB.
    assert!(
        snapshot
            .tool_activity
            .invocations
            .iter()
            .any(|invocation| invocation.class.mutates_worktree())
    );
    assert!(!snapshot.tool_activity.terminals.is_empty());
    assert!(snapshot.changes.file("hello.txt").is_some());
}
