#![allow(clippy::missing_errors_doc)]

//! Application-level session launch workflow.
//!
//! This crate sits above [`session_manager`]: the manager owns one isolated
//! Copilot runtime and its SDK event handling, while the orchestrator decides
//! where a new session runs, how it is named, how its kickoff is delivered, and
//! whether launching it should change application selection.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use app_model::{PromptAttachment, SessionKind, SessionLocation, TitleSource};
use git_service::GitService;
use session_manager::{CreateSessionRequest, SessionHandle, SessionManager, SessionRoots};
use thiserror::Error;

/// Whether a launch is visible navigation or background orchestration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LaunchOrigin {
    /// A desktop action that should activate the session after kickoff succeeds.
    UserActivation,
    /// A background caller that must not change application selection or focus.
    Headless,
}

/// How the first user-visible title is chosen.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LaunchTitle {
    /// Generate a semantic title when a worktree needs a name, otherwise use a
    /// deterministic prompt-derived fallback and refine it after launch.
    Automatic,
    /// Preserve a title and its provenance supplied by the caller.
    Provided { title: String, source: TitleSource },
}

/// Complete typed input for creating and starting a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchRequest {
    pub project_path: PathBuf,
    pub repository_root: Option<PathBuf>,
    /// Configured root under which a new managed worktree should be created.
    pub worktrees_root: PathBuf,
    pub kind: SessionKind,
    pub location: SessionLocation,
    pub prompt: String,
    pub attachments: Vec<PromptAttachment>,
    pub model: Option<String>,
    pub mode: String,
    pub agent: Option<String>,
    pub reasoning_effort: Option<String>,
    pub context_tier: Option<String>,
    pub base_ref: Option<String>,
    pub title: LaunchTitle,
    pub origin: LaunchOrigin,
}

/// Observable milestones for launch UIs. Headless callers may ignore them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LaunchProgress {
    CreatingWorktree,
    WorktreeReady(PathBuf),
}

/// A successfully created and started session.
#[derive(Clone)]
pub struct LaunchResult {
    pub handle: SessionHandle,
    pub project_path: PathBuf,
    pub repository_root: Option<PathBuf>,
    pub title: String,
    pub title_source: TitleSource,
    pub branch: Option<String>,
    pub message_id: String,
    pub origin: LaunchOrigin,
}

/// A compensation action that could not safely finish.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CleanupFailure {
    pub operation: &'static str,
    pub path: Option<PathBuf>,
    pub error: String,
}

/// Launch stage that failed before compensation ran.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LaunchStage {
    Worktree,
    Runtime,
    InitialMode,
    Kickoff,
    Activation,
}

impl std::fmt::Display for LaunchStage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Worktree => "worktree creation",
            Self::Runtime => "runtime creation",
            Self::InitialMode => "initial mode setup",
            Self::Kickoff => "kickoff submission",
            Self::Activation => "session activation",
        })
    }
}

/// A launch failure plus any work compensation deliberately preserved.
#[derive(Debug, Error)]
#[error("{stage} failed: {error}{cleanup_summary}")]
pub struct LaunchError {
    pub stage: LaunchStage,
    pub error: String,
    pub cleanup: Vec<CleanupFailure>,
    cleanup_summary: String,
}

impl LaunchError {
    fn new(stage: LaunchStage, error: impl Into<String>) -> Self {
        Self {
            stage,
            error: error.into(),
            cleanup: Vec::new(),
            cleanup_summary: String::new(),
        }
    }

    fn with_cleanup(mut self, cleanup: Vec<CleanupFailure>) -> Self {
        if !cleanup.is_empty() {
            let _ = write!(
                self.cleanup_summary,
                "; compensation incomplete: {}",
                cleanup
                    .iter()
                    .map(|failure| format!("{}: {}", failure.operation, failure.error))
                    .collect::<Vec<_>>()
                    .join("; ")
            );
        }
        self.cleanup = cleanup;
        self
    }
}

#[derive(Clone, Debug)]
struct CreatedWorktree {
    repository: PathBuf,
    path: PathBuf,
    branch: String,
}

#[derive(Clone, Debug)]
struct Workspace {
    path: PathBuf,
    created: Option<CreatedWorktree>,
}

enum SelectionRollback {
    Unchanged,
    Restore(Option<String>),
}

/// Shared launch service used by both interactive and headless callers.
#[derive(Clone)]
pub struct SessionOrchestrator {
    manager: Arc<SessionManager>,
    roots: SessionRoots,
}

impl SessionOrchestrator {
    #[must_use]
    pub fn new(manager: Arc<SessionManager>, roots: SessionRoots) -> Self {
        Self { manager, roots }
    }

    /// Create an isolated runtime, apply caller-requested activation, and
    /// deliver its kickoff.
    pub async fn launch(
        &self,
        request: LaunchRequest,
        mut on_progress: impl FnMut(LaunchProgress),
    ) -> Result<LaunchResult, LaunchError> {
        let previous_selection = self.previous_selection(request.origin)?;
        let repository = request
            .repository_root
            .clone()
            .unwrap_or_else(|| request.project_path.clone());
        let creates_worktree = request.kind == SessionKind::Project
            && request.location == SessionLocation::NewWorktree
            && GitService::new(&repository).is_worktree();
        let (title, title_source, refine_title) = self
            .select_title(&request, creates_worktree, &repository)
            .await;

        if creates_worktree {
            on_progress(LaunchProgress::CreatingWorktree);
        }
        let workspace = resolve_workspace(&request, &title, &repository)
            .map_err(|error| LaunchError::new(LaunchStage::Worktree, error))?;
        if workspace.created.is_some() {
            on_progress(LaunchProgress::WorktreeReady(workspace.path.clone()));
        }

        let handle = match self
            .create_runtime(&request, &workspace.path, &title, title_source)
            .await
        {
            Ok(handle) => handle,
            Err(error) => {
                let cleanup = self
                    .compensate(
                        None,
                        workspace.created.as_ref(),
                        &SelectionRollback::Unchanged,
                    )
                    .await;
                return Err(
                    LaunchError::new(LaunchStage::Runtime, error.to_string()).with_cleanup(cleanup)
                );
            }
        };

        if let Err(error) = handle.set_mode(request.mode.clone()).await {
            let cleanup = self
                .compensate(
                    Some(handle.id()),
                    workspace.created.as_ref(),
                    &SelectionRollback::Unchanged,
                )
                .await;
            return Err(
                LaunchError::new(LaunchStage::InitialMode, error.to_string()).with_cleanup(cleanup),
            );
        }

        self.activate(
            request.origin,
            &handle,
            workspace.created.as_ref(),
            &previous_selection,
        )
        .await?;

        let message_id = match handle
            .send_with_attachments(request.prompt.clone(), request.attachments)
            .await
        {
            Ok(message_id) => message_id,
            Err(error) => {
                let cleanup = self
                    .compensate(
                        Some(handle.id()),
                        workspace.created.as_ref(),
                        &previous_selection,
                    )
                    .await;
                return Err(
                    LaunchError::new(LaunchStage::Kickoff, error.to_string()).with_cleanup(cleanup)
                );
            }
        };

        if refine_title {
            let manager = self.manager.clone();
            let session_id = handle.id().to_owned();
            let prompt = request.prompt.clone();
            tokio::spawn(async move {
                if let Err(error) = manager.generate_session_title(&session_id, &prompt).await {
                    tracing::warn!(%error, %session_id, "session title generation failed");
                }
            });
        }

        Ok(LaunchResult {
            handle,
            project_path: workspace.path,
            repository_root: request.repository_root,
            title,
            title_source,
            branch: workspace.created.map(|created| created.branch),
            message_id,
            origin: request.origin,
        })
    }

    fn previous_selection(&self, origin: LaunchOrigin) -> Result<SelectionRollback, LaunchError> {
        if origin == LaunchOrigin::Headless {
            return Ok(SelectionRollback::Unchanged);
        }
        self.manager
            .selected_session()
            .map(SelectionRollback::Restore)
            .map_err(|error| LaunchError::new(LaunchStage::Activation, error.to_string()))
    }

    async fn activate(
        &self,
        origin: LaunchOrigin,
        handle: &SessionHandle,
        worktree: Option<&CreatedWorktree>,
        previous_selection: &SelectionRollback,
    ) -> Result<(), LaunchError> {
        if origin == LaunchOrigin::Headless {
            return Ok(());
        }
        if let Err(error) = self.manager.set_selected_session(Some(handle.id())) {
            let cleanup = self
                .compensate(Some(handle.id()), worktree, previous_selection)
                .await;
            return Err(
                LaunchError::new(LaunchStage::Activation, error.to_string()).with_cleanup(cleanup)
            );
        }
        Ok(())
    }

    async fn create_runtime(
        &self,
        request: &LaunchRequest,
        project_path: &Path,
        title: &str,
        title_source: TitleSource,
    ) -> session_manager::Result<SessionHandle> {
        self.manager
            .create_session(CreateSessionRequest {
                project_path: project_path.to_owned(),
                title: title.to_owned(),
                title_source,
                model: request.model.clone(),
                mode: Some(request.mode.clone()),
                agent: request.agent.clone(),
                reasoning_effort: request.reasoning_effort.clone(),
                context_tier: request.context_tier.clone(),
                base_ref: request.base_ref.clone(),
                repository_root: request
                    .repository_root
                    .as_ref()
                    .map(|path| path.to_string_lossy().into_owned()),
                kind: request.kind,
                unattended: false,
            })
            .await
    }

    async fn select_title(
        &self,
        request: &LaunchRequest,
        creates_worktree: bool,
        repository: &Path,
    ) -> (String, TitleSource, bool) {
        if let LaunchTitle::Provided { title, source } = &request.title {
            return (title.clone(), *source, false);
        }
        let fallback = fallback_title(&request.prompt);
        if !creates_worktree {
            return (fallback, TitleSource::Fallback, true);
        }
        match self
            .manager
            .generate_task_title(&request.prompt, request.model.as_deref(), repository)
            .await
        {
            Ok(Some(title)) => (title, TitleSource::Generated, false),
            Ok(None) => (fallback, TitleSource::Fallback, true),
            Err(error) => {
                tracing::warn!(%error, "worktree name generation failed");
                (fallback, TitleSource::Fallback, true)
            }
        }
    }

    async fn compensate(
        &self,
        app_session_id: Option<&str>,
        worktree: Option<&CreatedWorktree>,
        selection: &SelectionRollback,
    ) -> Vec<CleanupFailure> {
        let mut failures = Vec::new();
        if let Some(id) = app_session_id {
            if let Err(error) = self.manager.close_session(id).await {
                failures.push(CleanupFailure {
                    operation: "stop failed runtime",
                    path: None,
                    error: error.to_string(),
                });
            }
            let roots_without_worktrees = SessionRoots {
                worktrees: None,
                attachments: self.roots.attachments.clone(),
                runtime_state: self.roots.runtime_state.clone(),
            };
            if let Err(error) = self
                .manager
                .delete_session(id, &roots_without_worktrees)
                .await
            {
                failures.push(CleanupFailure {
                    operation: "remove failed session",
                    path: None,
                    error: error.to_string(),
                });
            }
        }
        if let Some(worktree) = worktree {
            failures.extend(cleanup_worktree(worktree));
        }
        if let SelectionRollback::Restore(selection) = selection
            && let Err(error) = self.manager.set_selected_session(selection.as_deref())
        {
            failures.push(CleanupFailure {
                operation: "restore session selection",
                path: None,
                error: error.to_string(),
            });
        }
        failures
    }
}

fn resolve_workspace(
    request: &LaunchRequest,
    title: &str,
    repository: &Path,
) -> Result<Workspace, String> {
    if request.kind.is_chat()
        || request.location == SessionLocation::LocalRepository
        || !GitService::new(repository).is_worktree()
    {
        return Ok(Workspace {
            path: request.project_path.clone(),
            created: None,
        });
    }
    let worktrees_root = &request.worktrees_root;
    let service = GitService::new(repository);
    let base = request
        .base_ref
        .clone()
        .or_else(|| service.default_branch())
        .unwrap_or_else(|| "HEAD".to_owned());
    if let Err(error) = service.fetch_base_ref(&base) {
        tracing::warn!(%error, base_ref = %base, "failed to refresh worktree base; using cached ref");
    }
    let namespace = repository_worktree_namespace(worktrees_root, repository)?;
    let branch = unique_worktree_branch(&service, title, &namespace);
    let path = worktree_path(&namespace, &branch)?;
    service
        .create_worktree(&path, &branch, &base)
        .map_err(|error| format!("failed to create session worktree: {error}"))?;
    Ok(Workspace {
        path: path.clone(),
        created: Some(CreatedWorktree {
            repository: repository.to_owned(),
            path,
            branch,
        }),
    })
}

fn cleanup_worktree(worktree: &CreatedWorktree) -> Vec<CleanupFailure> {
    let mut failures = Vec::new();
    let worktree_git = GitService::new(&worktree.path);
    if !worktree.path.exists() || !worktree_git.is_worktree() {
        failures.push(CleanupFailure {
            operation: "inspect failed worktree",
            path: Some(worktree.path.clone()),
            error: "worktree is missing or no longer registered; preserved branch".to_owned(),
        });
        return failures;
    }
    if !worktree_git.is_clean() {
        failures.push(CleanupFailure {
            operation: "remove failed worktree",
            path: Some(worktree.path.clone()),
            error: "worktree has changes and was preserved".to_owned(),
        });
        return failures;
    }

    let repository_git = GitService::new(&worktree.repository);
    if let Err(error) = repository_git.remove_worktree(&worktree.path) {
        failures.push(CleanupFailure {
            operation: "remove failed worktree",
            path: Some(worktree.path.clone()),
            error: error.to_string(),
        });
        return failures;
    }
    match repository_git.delete_branch_if_merged(&worktree.branch) {
        Ok(true) => {}
        Ok(false) => failures.push(CleanupFailure {
            operation: "remove failed branch",
            path: Some(worktree.repository.clone()),
            error: format!(
                "branch {} contains unmerged work and was preserved",
                worktree.branch
            ),
        }),
        Err(error) => failures.push(CleanupFailure {
            operation: "remove failed branch",
            path: Some(worktree.repository.clone()),
            error: error.to_string(),
        }),
    }
    failures
}

/// A branch name derived from the semantic session title, made unique in both
/// the repository and GCABB's managed worktree directory.
fn unique_worktree_branch(service: &GitService, title: &str, namespace: &Path) -> String {
    let slug = slugify(title);
    let candidate = format!("gcabb/{slug}");
    if worktree_name_available(service, &candidate, namespace) {
        return candidate;
    }
    for suffix in 2..100 {
        let candidate = format!("gcabb/{slug}-{suffix}");
        if worktree_name_available(service, &candidate, namespace) {
            return candidate;
        }
    }
    format!("gcabb/{slug}-{}", timestamp())
}

fn worktree_name_available(service: &GitService, branch: &str, namespace: &Path) -> bool {
    !service.branch_exists(branch) && !worktree_candidate_path(namespace, branch).exists()
}

/// Location on disk for a session worktree, outside the repository so it never
/// appears as untracked content in the changes view.
fn worktree_path(namespace: &Path, branch: &str) -> Result<PathBuf, String> {
    let path = worktree_candidate_path(namespace, branch);
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
    Ok(path)
}

fn worktree_candidate_path(namespace: &Path, branch: &str) -> PathBuf {
    namespace.join(branch.replace('/', "-"))
}

/// Stable, readable directory assigned to one repository.
///
/// Repositories named `gcabb` receive `gcabb`, `gcabb-2`, and so on. The
/// hidden owner file keeps that assignment stable without putting path hashes
/// into every worktree name.
fn repository_worktree_namespace(
    worktrees_root: &Path,
    repository: &Path,
) -> Result<PathBuf, String> {
    let canonical_repository = repository
        .canonicalize()
        .unwrap_or_else(|_| repository.to_owned());
    let repository_name = canonical_repository
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("repository");
    let base = repository_name.to_owned();
    let owner = canonical_repository.to_string_lossy();

    for suffix in 1_u32.. {
        let name = if suffix == 1 {
            base.clone()
        } else {
            format!("{base}-{suffix}")
        };
        let namespace = worktrees_root.join(name);
        let marker = namespace.join(".gcabb-repository");
        let created = !namespace.exists();
        if created {
            std::fs::create_dir_all(&namespace)
                .map_err(|error| format!("failed to create {}: {error}", namespace.display()))?;
        }
        if std::fs::read_to_string(&marker).is_ok_and(|stored| stored == owner) {
            return Ok(namespace);
        }

        let may_claim = if created {
            true
        } else {
            let entries = std::fs::read_dir(&namespace)
                .map_err(|error| format!("failed to read {}: {error}", namespace.display()))?;
            let mut roots = entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.is_dir())
                .filter(|path| GitService::new(path).is_worktree())
                .map(|path| GitService::new(path).repository_root());
            roots
                .next()
                .is_some_and(|root| root == canonical_repository)
                && roots.all(|root| root == canonical_repository)
        };
        if may_claim && claim_repository_namespace(&marker, owner.as_bytes())? {
            return Ok(namespace);
        }
    }
    unreachable!("an unbounded numeric namespace always has a candidate")
}

fn claim_repository_namespace(marker: &Path, owner: &[u8]) -> Result<bool, String> {
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(marker)
    {
        Ok(mut file) => {
            if let Err(error) = std::io::Write::write_all(&mut file, owner) {
                let _ = std::fs::remove_file(marker);
                return Err(format!("failed to write {}: {error}", marker.display()));
            }
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            Ok(std::fs::read(marker).is_ok_and(|stored| stored == owner))
        }
        Err(error) => Err(format!("failed to create {}: {error}", marker.display())),
    }
}

fn slugify(value: &str) -> String {
    let mut slug = String::new();
    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            slug.extend(character.to_lowercase());
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-');
    let slug: String = slug.chars().take(50).collect();
    let slug = slug.trim_matches('-').to_owned();
    if slug.is_empty() {
        "session".to_owned()
    } else {
        slug
    }
}

fn fallback_title(prompt: &str) -> String {
    let title = prompt
        .split_whitespace()
        .take(7)
        .collect::<Vec<_>>()
        .join(" ");
    if title.is_empty() {
        "New session".to_owned()
    } else if title.chars().count() > 56 {
        title.chars().take(53).collect::<String>() + "..."
    } else {
        title
    }
}

fn timestamp() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis())
        .to_string()
}
