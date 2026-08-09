use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use app_model::{
    ContextWindowOption, InteractionKind, InteractionResponse, ProjectMetadata, SessionKind,
    SessionLocation, SessionSnapshot, SessionStatus, TranscriptRole, TranscriptState,
};
use copilot_provider::{CopilotProvider, ProviderCompatibility};
use diagnostics::{TracingDiagnostics, init_tracing};
use git_service::GitService;
use gpui::prelude::FluentBuilder;
use gpui::{
    App, AppContext, Bounds, Context, Entity, Focusable, InteractiveElement, IntoElement,
    KeyBinding, MouseButton, ParentElement, PathPromptOptions, Render, Role, SharedString,
    StatefulInteractiveElement, Styled, TitlebarOptions, Window, WindowBounds, WindowOptions,
    actions, div, px, rgb, size,
};
use session_manager::{
    CreateSessionRequest, RestoreFailure, SessionHandle, SessionManager, WorktreeOutcome,
};
use storage::Storage;
use tokio::sync::watch;
use ui_components::{InputSubmitted, TextInput, bind_text_input_keys};

const BACKGROUND: u32 = 0x000d_1117;
const SIDEBAR: u32 = 0x0016_1b22;
const PANEL: u32 = 0x000d_1117;
const ELEVATED: u32 = 0x0021_262d;
const SUBTLE: u32 = 0x001b_222c;
const BORDER: u32 = 0x0030_363d;
const PRIMARY: u32 = 0x00f0_f3f6;
const MUTED: u32 = 0x008b_949e;
const GREEN: u32 = 0x003f_b950;
const BLUE: u32 = 0x0058_a6ff;
const AMBER: u32 = 0x00d2_9900;
const RED: u32 = 0x00f8_5161;
const COMPACT_WIDTH: f32 = 920.0;
/// Vertical budget for the detail blocks inside one tool entry.
const ENTRY_DETAIL_BUDGET: f32 = 480.0;
/// The command never takes more than a third of that budget, so output — the
/// part worth reading — always gets the majority.
const COMMAND_BLOCK_HEIGHT: f32 = ENTRY_DETAIL_BUDGET / 3.0;

/// Desktop-environment application identifier. On Wayland this becomes the
/// `xdg_toplevel` app ID and on X11 the `WM_CLASS`; both are used to match the
/// installed `com.constructomech.gcabb.desktop` entry that supplies the icon.
const APP_ID: &str = "com.constructomech.gcabb";

actions!(gcabb, [DismissPopup, FocusNext, FocusPrevious]);
enum ServiceUpdate {
    Ready {
        compatibility: ProviderCompatibility,
        projects: Vec<ProjectMetadata>,
        restored: Vec<SessionHandle>,
        failures: Vec<RestoreFailure>,
        selected_session: Option<String>,
    },
    SessionAdded(SessionHandle),
    SessionsDiscovered(Vec<SessionHandle>),
    /// A session was deleted and must be dropped from the UI.
    SessionDeleted(String),
    /// The configured project list changed, with the project to select next.
    ProjectsChanged {
        projects: Vec<ProjectMetadata>,
        selected: Option<String>,
    },
    PromptAccepted,
    ActionFailed(String),
    Failed(String),
}

enum ServiceCommand {
    Submit {
        app_session_id: Option<String>,
        prompt: String,
        project_path: PathBuf,
        model: Option<String>,
        mode: String,
        reasoning_effort: Option<String>,
        context_tier: Option<String>,
        /// Git ref new sessions compare their changes against.
        base_ref: Option<String>,
        /// Repository new sessions group under.
        repository_root: Option<String>,
        /// Whether to create a project session or a standalone chat.
        kind: SessionKind,
        /// Where a new project session should run.
        location: SessionLocation,
    },
    Cancel {
        app_session_id: String,
    },
    Close {
        app_session_id: String,
    },
    Resume {
        app_session_id: String,
    },
    Respond {
        app_session_id: String,
        interaction_id: String,
        response: InteractionResponse,
    },
    SetModel {
        app_session_id: String,
        model: String,
        reasoning_effort: Option<String>,
        context_tier: Option<String>,
    },
    SetMode {
        app_session_id: String,
        mode: String,
    },
    SetReasoningEffort {
        app_session_id: String,
        effort: String,
    },
    SetContextTier {
        app_session_id: String,
        tier: String,
    },
    Select {
        app_session_id: Option<String>,
    },
    RenameSession {
        app_session_id: String,
        title: String,
    },
    DeleteSession {
        app_session_id: String,
    },
    /// Register a directory chosen by the user as a project.
    AddProject {
        path: PathBuf,
    },
    RemoveProject {
        project_id: String,
    },
    Stop,
}

struct AppService {
    updates: Receiver<ServiceUpdate>,
    commands: Sender<ServiceCommand>,
    stopped: Receiver<()>,
}

impl AppService {
    #[allow(clippy::too_many_lines)]
    fn start(project_root: PathBuf, database_path: PathBuf) -> Self {
        let (update_tx, updates) = channel();
        let (commands, command_rx) = channel();
        let (stopped_tx, stopped) = channel();
        thread::Builder::new()
            .name("gcabb-services".to_owned())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .thread_name("gcabb-worker")
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = update_tx.send(ServiceUpdate::Failed(format!(
                            "failed to create async runtime: {error}"
                        )));
                        let _ = stopped_tx.send(());
                        return;
                    }
                };
                let storage = match Storage::open(&database_path) {
                    Ok(storage) => Arc::new(storage),
                    Err(error) => {
                        let _ = update_tx.send(ServiceUpdate::Failed(format!(
                            "failed to open {}: {error}",
                            database_path.display()
                        )));
                        let _ = stopped_tx.send(());
                        return;
                    }
                };
                let diagnostics = Arc::new(TracingDiagnostics);
                let provider = Arc::new(CopilotProvider::new(
                    project_root.clone(),
                    diagnostics.clone(),
                ));
                let manager = Arc::new(SessionManager::new(provider, storage, diagnostics));
                let worktrees = worktrees_root();
                // Projects are configured by the user, not inferred from the
                // launch directory. Auto-registering the launch repository
                // would silently re-add a project the user had removed.

                // Fold projects and sessions recorded by earlier builds, which
                // registered one project per worktree, into their repository.
                match manager.adopt_repository_roots(|path| {
                    let path = Path::new(path);
                    path.is_dir()
                        .then(|| repository_root(path).to_string_lossy().into_owned())
                }) {
                    Ok(0) => {}
                    Ok(count) => {
                        tracing::info!(count, "associated sessions with their repository");
                    }
                    Err(error) => {
                        tracing::warn!(%error, "failed to adopt repository roots");
                    }
                }

                match runtime.block_on(manager.start()) {
                    Ok((compatibility, report)) => {
                        let projects = manager.projects().unwrap_or_else(|error| {
                            tracing::error!(%error, "failed to list projects");
                            Vec::new()
                        });
                        let selected_session = manager.selected_session().unwrap_or(None);
                        let _ = update_tx.send(ServiceUpdate::Ready {
                            compatibility,
                            projects,
                            restored: report.restored,
                            failures: report.failed,
                            selected_session,
                        });
                    }
                    Err(error) => {
                        let _ = update_tx.send(ServiceUpdate::Failed(format!(
                            "Copilot provider startup failed: {error}"
                        )));
                    }
                }

                while let Ok(command) = command_rx.recv() {
                    if matches!(command, ServiceCommand::Stop) {
                        let _ = runtime.block_on(manager.stop());
                        break;
                    }
                    // Project changes publish a project list rather than a
                    // session, so they are handled before the session commands.
                    match command {
                        ServiceCommand::DeleteSession { app_session_id } => {
                            match runtime
                                .block_on(manager.delete_session(&app_session_id, Some(&worktrees)))
                            {
                                Ok(deletion) => {
                                    let _ =
                                        update_tx.send(ServiceUpdate::SessionDeleted(deletion.id));
                                    // A preserved or unremovable worktree is
                                    // worth saying out loud so it cannot be
                                    // orphaned silently.
                                    if let Some(notice) =
                                        deletion.worktree.as_ref().and_then(WorktreeOutcome::notice)
                                    {
                                        let _ = update_tx.send(ServiceUpdate::ActionFailed(notice));
                                    }
                                }
                                Err(error) => {
                                    let _ = update_tx
                                        .send(ServiceUpdate::ActionFailed(error.to_string()));
                                }
                            }
                        }
                        ServiceCommand::AddProject { path } => {
                            match register_directory_as_project(&manager, &path) {
                                Ok(selected) => {
                                    let projects = manager.projects().unwrap_or_default();
                                    let _ = update_tx.send(ServiceUpdate::ProjectsChanged {
                                        projects,
                                        selected: Some(selected),
                                    });
                                }
                                Err(error) => {
                                    let _ = update_tx.send(ServiceUpdate::ActionFailed(error));
                                }
                            }
                        }
                        ServiceCommand::RemoveProject { project_id } => {
                            if let Err(error) = manager.remove_project(&project_id) {
                                let _ =
                                    update_tx.send(ServiceUpdate::ActionFailed(error.to_string()));
                            } else {
                                let projects = manager.projects().unwrap_or_default();
                                let selected = projects.first().map(|project| project.path.clone());
                                let _ = update_tx
                                    .send(ServiceUpdate::ProjectsChanged { projects, selected });
                            }
                        }
                        command => {
                            let is_submit = matches!(&command, ServiceCommand::Submit { .. });
                            match runtime
                                .block_on(handle_service_command(&manager, command, &worktrees))
                            {
                                Ok(Some(handle)) => {
                                    let _ = update_tx.send(ServiceUpdate::SessionAdded(handle));
                                    if is_submit {
                                        let _ = update_tx.send(ServiceUpdate::PromptAccepted);
                                    }
                                }
                                Ok(None) => {
                                    if is_submit {
                                        let _ = update_tx.send(ServiceUpdate::PromptAccepted);
                                    }
                                }
                                Err(error) => {
                                    let _ = update_tx.send(ServiceUpdate::ActionFailed(error));
                                    let sessions = runtime.block_on(manager.sessions());
                                    let _ =
                                        update_tx.send(ServiceUpdate::SessionsDiscovered(sessions));
                                }
                            }
                        }
                    }
                }
                let _ = stopped_tx.send(());
            })
            .expect("failed to start GCABB service thread");
        Self {
            updates,
            commands,
            stopped,
        }
    }

    fn failed(error: String) -> Self {
        let (update_tx, updates) = channel();
        let (commands, _command_rx) = channel();
        let (stopped_tx, stopped) = channel();
        let _ = update_tx.send(ServiceUpdate::Failed(error));
        let _ = stopped_tx.send(());
        Self {
            updates,
            commands,
            stopped,
        }
    }

    /// A service with no backing thread, plus the command receiver.
    ///
    /// View tests drive real UI code but must not start a Copilot provider, so
    /// commands are captured and asserted on instead of executed.
    #[cfg(test)]
    fn for_test() -> (Self, Receiver<ServiceCommand>) {
        let (_update_tx, updates) = channel();
        let (commands, command_rx) = channel();
        let (_stopped_tx, stopped) = channel();
        (
            Self {
                updates,
                commands,
                stopped,
            },
            command_rx,
        )
    }
}

#[allow(clippy::too_many_lines)]
async fn handle_service_command(
    manager: &SessionManager,
    command: ServiceCommand,
    worktrees_root: &Path,
) -> Result<Option<SessionHandle>, String> {
    let mut created = None;
    match command {
        ServiceCommand::Submit {
            app_session_id,
            prompt,
            project_path,
            model,
            mode,
            reasoning_effort,
            context_tier,
            base_ref,
            repository_root,
            kind,
            location,
        } => {
            let handle = if let Some(id) = app_session_id {
                manager
                    .session(&id)
                    .await
                    .map_err(|error| error.to_string())?
            } else {
                let initial_mode = mode.clone();
                let title = session_title(&prompt);
                // A worktree session runs in its own checkout, created before
                // the provider session so the CLI starts in the right place.
                let project_path = resolve_session_workspace(
                    location,
                    kind,
                    &project_path,
                    repository_root.as_deref(),
                    base_ref.as_deref(),
                    &title,
                    worktrees_root,
                )?;
                let handle = manager
                    .create_session(CreateSessionRequest {
                        project_path,
                        title,
                        model,
                        mode: Some(mode),
                        reasoning_effort,
                        context_tier,
                        base_ref,
                        repository_root,
                        kind,
                    })
                    .await
                    .map_err(|error| error.to_string())?;
                created = Some(handle.clone());
                handle
                    .set_mode(initial_mode)
                    .await
                    .map_err(|error| error.to_string())?;
                handle
            };
            manager
                .set_selected_session(Some(handle.id()))
                .map_err(|error| error.to_string())?;
            handle
                .send(prompt)
                .await
                .map_err(|error| error.to_string())?;
        }
        ServiceCommand::Cancel { app_session_id } => manager
            .session(&app_session_id)
            .await
            .map_err(|error| error.to_string())?
            .cancel()
            .await
            .map_err(|error| error.to_string())?,
        ServiceCommand::Close { app_session_id } => manager
            .close_session(&app_session_id)
            .await
            .map_err(|error| error.to_string())?,
        ServiceCommand::Resume { app_session_id } => {
            created = Some(
                manager
                    .resume_closed_session(&app_session_id)
                    .await
                    .map_err(|error| error.to_string())?,
            );
        }
        ServiceCommand::Respond {
            app_session_id,
            interaction_id,
            response,
        } => manager
            .session(&app_session_id)
            .await
            .map_err(|error| error.to_string())?
            .respond(interaction_id, response)
            .await
            .map_err(|error| error.to_string())?,
        ServiceCommand::SetModel {
            app_session_id,
            model,
            reasoning_effort,
            context_tier,
        } => manager
            .session(&app_session_id)
            .await
            .map_err(|error| error.to_string())?
            .set_model_with_options(model, reasoning_effort, context_tier)
            .await
            .map_err(|error| error.to_string())?,
        ServiceCommand::SetMode {
            app_session_id,
            mode,
        } => manager
            .session(&app_session_id)
            .await
            .map_err(|error| error.to_string())?
            .set_mode(mode)
            .await
            .map_err(|error| error.to_string())?,
        ServiceCommand::SetReasoningEffort {
            app_session_id,
            effort,
        } => manager
            .session(&app_session_id)
            .await
            .map_err(|error| error.to_string())?
            .set_reasoning_effort(effort)
            .await
            .map_err(|error| error.to_string())?,
        ServiceCommand::SetContextTier {
            app_session_id,
            tier,
        } => manager
            .session(&app_session_id)
            .await
            .map_err(|error| error.to_string())?
            .set_context_tier(tier)
            .await
            .map_err(|error| error.to_string())?,
        ServiceCommand::RenameSession {
            app_session_id,
            title,
        } => manager
            .rename_session(&app_session_id, &title)
            .await
            .map_err(|error| error.to_string())?,
        ServiceCommand::Select { app_session_id } => manager
            .set_selected_session(app_session_id.as_deref())
            .map_err(|error| error.to_string())?,
        // Project commands publish a project list instead of a session and
        // are handled before this dispatch.
        ServiceCommand::AddProject { .. }
        | ServiceCommand::RemoveProject { .. }
        | ServiceCommand::DeleteSession { .. }
        | ServiceCommand::Stop => {}
    }
    Ok(created)
}

/// Register a user-chosen directory as a project.
///
/// The directory may be any folder on disk. When it is inside a git worktree
/// the repository root is registered instead, so adding a worktree and adding
/// its main checkout produce the same project rather than duplicates.
///
/// Returns the path that should become the selected project.
fn register_directory_as_project(manager: &SessionManager, path: &Path) -> Result<String, String> {
    if !path.is_dir() {
        return Err(format!("{} is not a directory", path.display()));
    }
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_owned());
    let root = repository_root(&canonical);
    let path_string = root.to_string_lossy().into_owned();
    let project = ProjectMetadata {
        id: path_string.clone(),
        path: path_string.clone(),
        name: root
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("Project")
            .to_owned(),
        default_branch: default_branch(&root),
        last_opened_at: timestamp(),
    };
    manager
        .register_project(&project)
        .map_err(|error| error.to_string())?;
    Ok(path_string)
}

/// Decide the directory a new session runs in.
///
/// `LocalRepository` uses the project directory as-is. `NewWorktree` creates a
/// linked worktree on a fresh branch so the session cannot disturb the
/// repository the developer is using, which is what makes parallel sessions in
/// one repository safe.
///
/// Chats and non-repository directories always run in place; there is nothing
/// to branch from.
fn resolve_session_workspace(
    location: SessionLocation,
    kind: SessionKind,
    project_path: &Path,
    repository_root: Option<&str>,
    base_ref: Option<&str>,
    title: &str,
    worktrees_root: &Path,
) -> Result<PathBuf, String> {
    if kind.is_chat() || location == SessionLocation::LocalRepository {
        return Ok(project_path.to_owned());
    }
    let repository = repository_root.map_or_else(|| project_path.to_owned(), PathBuf::from);
    let service = GitService::new(&repository);
    if !service.is_worktree() {
        // Not a repository, so there is nothing to create a worktree from.
        return Ok(project_path.to_owned());
    }

    let base = base_ref
        .map(str::to_owned)
        .or_else(|| default_branch(&repository))
        .unwrap_or_else(|| "HEAD".to_owned());
    let branch = unique_worktree_branch(&service, title);
    let path = worktree_path(worktrees_root, &repository, &branch)?;
    service
        .create_worktree(&path, &branch, &base)
        .map_err(|error| format!("failed to create session worktree: {error}"))?;
    Ok(path)
}

/// A branch name derived from the session title, made unique in the repository.
fn unique_worktree_branch(service: &GitService, title: &str) -> String {
    let slug = slugify(title);
    let candidate = format!("gcabb/{slug}");
    if !service.branch_exists(&candidate) {
        return candidate;
    }
    for suffix in 2..100 {
        let candidate = format!("gcabb/{slug}-{suffix}");
        if !service.branch_exists(&candidate) {
            return candidate;
        }
    }
    format!("gcabb/{slug}-{}", timestamp())
}

/// Location on disk for a session worktree, outside the repository so it never
/// appears as untracked content in the changes view.
fn worktree_path(
    worktrees_root: &Path,
    repository: &Path,
    branch: &str,
) -> Result<PathBuf, String> {
    let repository_name = repository
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("repository");
    let leaf = branch.replace('/', "-");
    let path = worktrees_root.join(repository_name);
    std::fs::create_dir_all(&path)
        .map_err(|error| format!("failed to create {}: {error}", path.display()))?;
    Ok(path.join(leaf))
}

/// Root directory session worktrees are created under.
///
/// Kept beside the application database so it follows `GCABB_DATA_DIR` during
/// development and never lands inside a repository.
fn worktrees_root() -> PathBuf {
    if let Some(path) = std::env::var_os("GCABB_DATA_DIR") {
        return PathBuf::from(path).join("worktrees");
    }
    dirs::data_local_dir().map_or_else(
        || PathBuf::from(".gcabb").join("worktrees"),
        |base| base.join("gcabb").join("worktrees"),
    )
}

/// Lowercase, hyphenated slug suitable for a git branch component.
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
    let slug: String = slug.chars().take(40).collect();
    let slug = slug.trim_matches('-').to_owned();
    if slug.is_empty() {
        "session".to_owned()
    } else {
        slug
    }
}

fn session_title(prompt: &str) -> String {
    let title = prompt
        .split_whitespace()
        .take(7)
        .collect::<Vec<_>>()
        .join(" ");
    if title.chars().count() > 56 {
        title.chars().take(53).collect::<String>() + "..."
    } else {
        title
    }
}

struct SessionProjection {
    handle: SessionHandle,
    receiver: watch::Receiver<Arc<SessionSnapshot>>,
    snapshot: Arc<SessionSnapshot>,
}

impl SessionProjection {
    fn new(handle: SessionHandle) -> Self {
        let receiver = handle.subscribe();
        let snapshot = receiver.borrow().clone();
        Self {
            handle,
            receiver,
            snapshot,
        }
    }

    #[cfg(test)]
    fn for_test(handle: SessionHandle) -> Self {
        Self::new(handle)
    }
}

enum StartupState {
    Starting,
    Ready(ProviderCompatibility),
    Failed(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ControlMenu {
    Project,
    Location,
    Mode,
    Model,
    Effort,
    Context,
}

/// Sentinel option value that opens the folder picker from the project menu.
const ADD_PROJECT_OPTION: &str = "\u{0}add-project";
/// Sentinel option value that switches the composer to a standalone chat.
const CHAT_OPTION: &str = "\u{0}chat";

/// An open session context menu, anchored at the click position.
struct SessionMenu {
    id: String,
    title: String,
    position: gpui::Point<gpui::Pixels>,
}

/// Phase 3 inspector tabs for the session side panel.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionPanel {
    Changes,
    Terminals,
    Capabilities,
}

impl SessionPanel {
    const ALL: [Self; 3] = [Self::Changes, Self::Terminals, Self::Capabilities];

    const fn label(self) -> &'static str {
        match self {
            Self::Changes => "Changes",
            Self::Terminals => "Terminals",
            Self::Capabilities => "Capabilities",
        }
    }

    const fn id(self) -> &'static str {
        match self {
            Self::Changes => "panel-changes",
            Self::Terminals => "panel-terminals",
            Self::Capabilities => "panel-capabilities",
        }
    }
}

struct SessionMvpView {
    startup: StartupState,
    projects: Vec<ProjectMetadata>,
    sessions: Vec<SessionProjection>,
    selected_session: Option<String>,
    /// Repository grouping key for the sidebar.
    selected_project: PathBuf,
    /// Directory new sessions run in.
    workspace_root: PathBuf,
    /// Directory GCABB was launched from, used when no project is selected.
    launch_workspace: PathBuf,
    /// Working directory chats run in, since chats have no repository.
    chats_workspace: PathBuf,
    /// Whether the composer will start a chat rather than a project session.
    composing_chat: bool,
    /// Where the next project session will run.
    draft_location: SessionLocation,
    /// Branch currently checked out in the selected project, refreshed when
    /// the selection changes so the composer never runs git per frame.
    project_branch: Option<String>,
    /// Scroll position of the transcript.
    transcript_scroll: gpui::ScrollHandle,
    /// Scroll positions of the detail blocks inside tool entries, keyed by
    /// block id so each keeps its position across renders.
    detail_scrolls: RefCell<HashMap<String, gpui::ScrollHandle>>,
    /// Transcript length last auto-scrolled for, so the view follows new
    /// output without fighting a user who has scrolled up to read.
    transcript_extent: (String, usize, usize),
    restore_failures: Vec<RestoreFailure>,
    updates: Receiver<ServiceUpdate>,
    commands: Sender<ServiceCommand>,
    branch: String,
    composer: Entity<TextInput>,
    interaction_input: Entity<TextInput>,
    draft_mode: String,
    draft_model: Option<String>,
    draft_effort: String,
    draft_context_tier: Option<String>,
    sidebar_open: bool,
    panel_open: bool,
    active_panel: SessionPanel,
    selected_change: Option<String>,
    open_control_menu: Option<ControlMenu>,
    /// Session whose context menu is open, and where to draw it.
    session_menu: Option<SessionMenu>,
    /// Session being renamed, if the rename dialog is open.
    renaming_session: Option<String>,
    rename_input: Entity<TextInput>,
    action_error: Option<String>,
    _poll_task: gpui::Task<()>,
}

impl SessionMvpView {
    #[allow(clippy::too_many_lines)]
    fn new(
        service: AppService,
        project_root: PathBuf,
        branch: String,
        chats_workspace: PathBuf,
        cx: &mut Context<Self>,
    ) -> Self {
        let commands = service.commands;
        let quit_commands = commands.clone();
        let stopped = Arc::new(Mutex::new(service.stopped));
        let background_executor = cx.background_executor().clone();
        cx.on_app_quit(move |_, _| {
            let quit_commands = quit_commands.clone();
            let stopped = stopped.clone();
            let background_executor = background_executor.clone();
            async move {
                let _ = quit_commands.send(ServiceCommand::Stop);
                for _ in 0..10 {
                    let is_stopped = stopped
                        .lock()
                        .map_or(true, |receiver| receiver.try_recv().is_ok());
                    if is_stopped {
                        break;
                    }
                    background_executor.timer(Duration::from_millis(10)).await;
                }
            }
        })
        .detach();

        let composer = cx.new(|cx| {
            TextInput::new(
                cx,
                "composer-input",
                "Ask anything, paste a URL, type / for commands, # for issues or & for sessions...",
            )
        });
        cx.subscribe(&composer, |view, _, event: &InputSubmitted, cx| {
            view.submit_prompt(event.text.clone());
            cx.notify();
        })
        .detach();
        let interaction_input =
            cx.new(|cx| TextInput::new(cx, "interaction-input", "Type your response..."));
        cx.subscribe(&interaction_input, |view, _, event: &InputSubmitted, cx| {
            view.submit_interaction(event.text.clone());
            view.interaction_input.update(cx, TextInput::clear);
            cx.notify();
        })
        .detach();
        let rename_input = cx.new(|cx| TextInput::new(cx, "rename-input", "Session name"));
        cx.subscribe(&rename_input, |view, _, event: &InputSubmitted, cx| {
            view.commit_rename(&event.text, cx);
        })
        .detach();

        let poll_task = cx.spawn(async move |view, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(33))
                    .await;
                if view
                    .update(cx, |view, cx| {
                        // Both sides must run, so avoid short-circuiting here.
                        let updated = view.apply_service_updates(cx);
                        let refreshed = view.refresh_snapshots();
                        if updated || refreshed {
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        });

        Self {
            startup: StartupState::Starting,
            projects: Vec::new(),
            sessions: Vec::new(),
            selected_session: None,
            selected_project: repository_root(&project_root),
            workspace_root: project_root.clone(),
            launch_workspace: project_root,
            chats_workspace,
            composing_chat: false,
            draft_location: SessionLocation::default(),
            project_branch: None,
            transcript_scroll: gpui::ScrollHandle::new(),
            detail_scrolls: RefCell::new(HashMap::new()),
            transcript_extent: (String::new(), 0, 0),
            restore_failures: Vec::new(),
            updates: service.updates,
            commands,
            branch,
            composer,
            interaction_input,
            draft_mode: "interactive".to_owned(),
            draft_model: None,
            draft_effort: "medium".to_owned(),
            draft_context_tier: None,
            sidebar_open: true,
            panel_open: false,
            active_panel: SessionPanel::Changes,
            selected_change: None,
            open_control_menu: None,
            session_menu: None,
            renaming_session: None,
            rename_input,
            action_error: None,
            _poll_task: poll_task,
        }
    }

    /// Drains pending service updates, returning whether any were applied so the
    /// caller can skip repainting when the poll tick found nothing to do.
    fn apply_service_updates(&mut self, cx: &mut Context<Self>) -> bool {
        let mut changed = false;
        loop {
            let update = match self.updates.try_recv() {
                Ok(update) => update,
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            };
            changed = true;
            match update {
                ServiceUpdate::Ready {
                    compatibility,
                    projects,
                    restored,
                    failures,
                    selected_session,
                } => {
                    self.startup = StartupState::Ready(compatibility);
                    self.projects = projects;
                    self.sessions = restored.into_iter().map(SessionProjection::new).collect();
                    self.selected_session = selected_session
                        .filter(|id| {
                            self.sessions
                                .iter()
                                .any(|session| session.handle.id() == id)
                        })
                        .or_else(|| {
                            self.sessions
                                .first()
                                .map(|session| session.snapshot.metadata.id.clone())
                        });
                    if let Some((project, workspace)) = self
                        .selected()
                        .filter(|session| !session.snapshot.metadata.is_chat())
                        .map(|session| {
                            (
                                PathBuf::from(session.snapshot.metadata.project_key()),
                                PathBuf::from(&session.snapshot.metadata.project_path),
                            )
                        })
                    {
                        self.selected_project = project;
                        self.workspace_root = workspace;
                    }
                    if self.projects.is_empty() {
                        self.composing_chat = true;
                    }
                    self.restore_failures = failures;
                }
                ServiceUpdate::SessionAdded(handle) => {
                    let id = handle.id().to_owned();
                    if let Some(index) = self
                        .sessions
                        .iter()
                        .position(|session| session.handle.id() == id)
                    {
                        self.sessions[index] = SessionProjection::new(handle);
                    } else {
                        self.sessions.insert(0, SessionProjection::new(handle));
                    }
                    self.selected_session = Some(id);
                }
                ServiceUpdate::SessionsDiscovered(handles) => {
                    for handle in handles {
                        let id = handle.id().to_owned();
                        if let Some(index) = self
                            .sessions
                            .iter()
                            .position(|session| session.handle.id() == id)
                        {
                            self.sessions[index] = SessionProjection::new(handle);
                        } else {
                            self.sessions.insert(0, SessionProjection::new(handle));
                        }
                    }
                }
                ServiceUpdate::ProjectsChanged { projects, selected } => {
                    self.apply_projects_changed(projects, selected, cx);
                }
                ServiceUpdate::SessionDeleted(id) => {
                    self.sessions.retain(|session| session.handle.id() != id);
                    if self.selected_session.as_deref() == Some(id.as_str()) {
                        self.selected_session = None;
                    }
                    if self.session_menu.as_ref().is_some_and(|menu| menu.id == id) {
                        self.session_menu = None;
                    }
                    if self.renaming_session.as_deref() == Some(id.as_str()) {
                        self.renaming_session = None;
                    }
                }
                ServiceUpdate::PromptAccepted => {
                    self.composer.update(cx, TextInput::clear);
                }
                ServiceUpdate::ActionFailed(error) => self.action_error = Some(error),
                ServiceUpdate::Failed(error) => self.startup = StartupState::Failed(error),
            }
        }
        changed
    }

    /// Adopt a new project list, selecting `selected` when one was given.
    fn apply_projects_changed(
        &mut self,
        projects: Vec<ProjectMetadata>,
        selected: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.projects = projects;
        if let Some(selected) = selected {
            self.select_project(&selected, cx);
            return;
        }
        // No projects are configured, so there is nothing to select. Falling
        // back to the launch directory made the pill advertise a project that
        // was not in the list; chat is the only target that needs no
        // configuration.
        self.composing_chat = true;
        self.selected_project.clone_from(&self.launch_workspace);
        self.workspace_root.clone_from(&self.launch_workspace);
        self.selected_session = None;
        let _ = self.commands.send(ServiceCommand::Select {
            app_session_id: None,
        });
    }

    /// Pulls any changed session snapshots, returning whether one actually moved.
    fn refresh_snapshots(&mut self) -> bool {
        let mut changed = false;
        for projection in &mut self.sessions {
            if projection.receiver.has_changed().unwrap_or(false) {
                projection.snapshot = projection.receiver.borrow_and_update().clone();
                changed = true;
            }
        }
        changed
    }

    fn selected(&self) -> Option<&SessionProjection> {
        let id = self.selected_session.as_deref()?;
        self.sessions
            .iter()
            .find(|session| session.handle.id() == id)
    }

    fn submit_prompt(&mut self, prompt: String) {
        self.action_error = None;
        let supported_efforts = self
            .draft_model
            .as_deref()
            .map_or_else(Vec::new, |model| self.supported_reasoning_efforts(model));
        // A chat has no repository, so it gets a neutral working directory and
        // no changes base.
        let (project_path, repository_root, base_ref, kind) = if self.targets_chat() {
            (self.chats_workspace.clone(), None, None, SessionKind::Chat)
        } else {
            (
                self.workspace_root.clone(),
                Some(self.selected_project.to_string_lossy().into_owned()),
                self.selected_project_base_ref(),
                SessionKind::Project,
            )
        };
        let _ = self.commands.send(ServiceCommand::Submit {
            app_session_id: self.selected_session.clone(),
            prompt,
            project_path,
            model: self.draft_model.clone(),
            mode: self.draft_mode.clone(),
            reasoning_effort: reasoning_effort_for_model(&supported_efforts, &self.draft_effort),
            context_tier: self.selectable_context_tier(),
            base_ref,
            repository_root,
            kind,
            location: self.draft_location,
        });
    }

    /// Whether the composer will act on a chat rather than a project.
    ///
    /// A selected session decides for itself; otherwise the draft state does.
    fn targets_chat(&self) -> bool {
        self.selected().map_or(self.composing_chat, |session| {
            session.snapshot.metadata.is_chat()
        })
    }

    /// Label for the composer's project pill.
    ///
    /// Chat mode must be visible here, otherwise choosing Chat changes state
    /// with no on-screen effect.
    fn composer_project_label(&self) -> String {
        if self.targets_chat() {
            return "Chat".to_owned();
        }
        self.projects
            .iter()
            .find(|project| Path::new(&project.path) == self.selected_project)
            .map_or_else(
                // The launch directory is not a project unless the user added
                // it, so naming it here would advertise a project that is not
                // in the picker.
                || "No project".to_owned(),
                |project| project.name.clone(),
            )
    }

    /// Keep the newest output in view as it arrives.
    ///
    /// Only scrolls when the transcript actually grew, so a user who has
    /// scrolled up to read earlier output is not yanked back to the bottom on
    /// every frame. Switching sessions also scrolls, so a session never opens
    /// showing the middle of a conversation.
    fn follow_transcript_tail(&mut self) {
        let Some(session) = self.selected() else {
            return;
        };
        let extent = (
            session.snapshot.metadata.id.clone(),
            session.snapshot.transcript.len(),
            session
                .snapshot
                .transcript
                .last()
                .map_or(0, |message| message.content.len()),
        );
        if extent == self.transcript_extent {
            return;
        }
        let switched_session = extent.0 != self.transcript_extent.0;
        let grew = extent.1 > self.transcript_extent.1 || extent.2 > self.transcript_extent.2;
        self.transcript_extent = extent;
        if switched_session || grew {
            self.transcript_scroll.scroll_to_bottom();
        }
    }

    /// Branch shown beside the location pill.
    ///
    /// A new worktree does not exist yet, so it names the base branch it will
    /// be created from. Running in the local repository names the branch that
    /// repository currently has checked out. Neither is the branch of the
    /// directory GCABB happened to be launched from.
    fn composer_branch_label(&self) -> String {
        let default_branch = self
            .projects
            .iter()
            .find(|project| Path::new(&project.path) == self.selected_project)
            .and_then(|project| project.default_branch.clone());
        match self.draft_location {
            SessionLocation::NewWorktree => default_branch
                .or_else(|| self.project_branch.clone())
                .unwrap_or_else(|| "HEAD".to_owned()),
            SessionLocation::LocalRepository => self
                .project_branch
                .clone()
                .or(default_branch)
                .unwrap_or_else(|| "HEAD".to_owned()),
        }
    }

    /// Start composing a standalone chat.
    fn new_chat(&mut self, cx: &mut Context<Self>) {
        self.open_control_menu = None;
        self.composing_chat = true;
        self.selected_session = None;
        self.action_error = None;
        self.composer.update(cx, TextInput::clear);
        let _ = self.commands.send(ServiceCommand::Select {
            app_session_id: None,
        });
        cx.notify();
    }

    /// Base ref new sessions in the selected project compare against.
    ///
    /// The repository's default branch is the natural base for a session
    /// worktree; sessions record it once so later movement on that branch does
    /// not silently change what the changes view reports. Falls back to
    /// resolving it directly when the project has none recorded.
    fn selected_project_base_ref(&self) -> Option<String> {
        self.projects
            .iter()
            .find(|project| Path::new(&project.path) == self.selected_project)
            .and_then(|project| project.default_branch.clone())
            .or_else(|| default_branch(&self.selected_project))
    }

    fn submit_interaction(&mut self, value: String) {
        let Some(session) = self.selected() else {
            return;
        };
        let Some(interaction) = session.snapshot.pending_interactions.first() else {
            return;
        };
        let _ = self.commands.send(ServiceCommand::Respond {
            app_session_id: session.handle.id().to_owned(),
            interaction_id: interaction.id.clone(),
            response: InteractionResponse::Submit {
                value: value.into(),
                freeform: true,
            },
        });
    }

    fn select_session(&mut self, id: String, cx: &mut Context<Self>) {
        self.open_control_menu = None;
        self.selected_session = Some(id);
        let _ = self.commands.send(ServiceCommand::Select {
            app_session_id: self.selected_session.clone(),
        });
        if let Some(controls) = self
            .selected()
            .map(|session| session.snapshot.controls.clone())
        {
            // Selecting a chat must leave the project selection alone: the
            // sidebar filters project sessions by it, so repointing it at the
            // chats directory hid every project session.
            if let Some((project, workspace)) = self
                .selected()
                .filter(|session| !session.snapshot.metadata.is_chat())
                .map(|session| {
                    (
                        PathBuf::from(session.snapshot.metadata.project_key()),
                        PathBuf::from(&session.snapshot.metadata.project_path),
                    )
                })
            {
                self.selected_project = project;
                self.workspace_root = workspace;
            }
            self.draft_mode = controls.mode.unwrap_or_else(|| "interactive".to_owned());
            self.draft_model = controls.model;
            self.draft_effort = controls
                .reasoning_effort
                .unwrap_or_else(|| "medium".to_owned());
            self.draft_context_tier = controls.context_tier;
        }
        cx.notify();
    }

    fn select_project(&mut self, path: &str, cx: &mut Context<Self>) {
        self.open_control_menu = None;
        // Choosing a project leaves chat mode. This is the single place that
        // means "the user picked a project", so adding a project, picking one
        // from the menu, and restoring a session all clear the flag here.
        self.composing_chat = false;
        self.selected_project = PathBuf::from(path);
        self.project_branch = git_output(Path::new(path), &["branch", "--show-current"]);
        // New sessions run in the project directory the user chose.
        self.workspace_root = PathBuf::from(path);
        self.selected_session = self
            .sessions
            .iter()
            .find(|session| session.snapshot.metadata.project_key() == path)
            .map(|session| session.handle.id().to_owned());
        if let Some(workspace) = self
            .selected()
            .map(|session| PathBuf::from(&session.snapshot.metadata.project_path))
        {
            self.workspace_root = workspace;
        }
        let _ = self.commands.send(ServiceCommand::Select {
            app_session_id: self.selected_session.clone(),
        });
        cx.notify();
    }

    /// Open the platform folder picker and register the chosen directory.
    fn add_project(&mut self, cx: &mut Context<Self>) {
        self.open_control_menu = None;
        self.action_error = None;
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Add project".into()),
        });
        cx.spawn(async move |view, cx| {
            let selection = match paths.await {
                Ok(Ok(paths)) => paths.and_then(|paths| paths.into_iter().next()),
                // The Linux picker goes through a desktop portal, which can
                // fail outright; surface that rather than silently doing
                // nothing.
                Ok(Err(error)) => {
                    let message = format!("could not open the folder picker: {error}");
                    let _ = view.update(cx, |view, cx| {
                        view.action_error = Some(message);
                        cx.notify();
                    });
                    return;
                }
                // The channel closes when the dialog is dismissed.
                Err(_) => None,
            };
            let Some(path) = selection else {
                return;
            };
            let _ = view.update(cx, |view, cx| {
                let _ = view.commands.send(ServiceCommand::AddProject { path });
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn remove_project(&mut self, project_id: String, cx: &mut Context<Self>) {
        self.action_error = None;
        let _ = self
            .commands
            .send(ServiceCommand::RemoveProject { project_id });
        cx.notify();
    }

    /// Open the context menu for a session at the pointer position.
    fn open_session_menu(
        &mut self,
        id: String,
        title: String,
        position: gpui::Point<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.open_control_menu = None;
        self.session_menu = Some(SessionMenu {
            id,
            title,
            position,
        });
        cx.notify();
    }

    fn dismiss_session_menu(&mut self, cx: &mut Context<Self>) {
        if self.session_menu.take().is_some() {
            cx.notify();
        }
    }

    /// Open the rename dialog, seeded with the session's current title.
    fn begin_rename(
        &mut self,
        id: String,
        title: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.session_menu = None;
        self.renaming_session = Some(id);
        self.rename_input
            .update(cx, |input, cx| input.set_value(title, cx));
        // Open ready to type rather than requiring a click into the field.
        let focus_handle = self.rename_input.read(cx).focus_handle(cx);
        window.focus(&focus_handle, cx);
        cx.notify();
    }

    fn commit_rename(&mut self, title: &str, cx: &mut Context<Self>) {
        let Some(app_session_id) = self.renaming_session.take() else {
            return;
        };
        let title = title.trim().to_owned();
        // An empty name would leave the session unidentifiable in the sidebar.
        if !title.is_empty() {
            let _ = self.commands.send(ServiceCommand::RenameSession {
                app_session_id: app_session_id.clone(),
                title: title.clone(),
            });
            if let Some(session) = self
                .sessions
                .iter_mut()
                .find(|session| session.handle.id() == app_session_id)
            {
                // Reflect the new name immediately; the actor's snapshot
                // follows once the command is applied.
                let mut snapshot = (*session.snapshot).clone();
                snapshot.metadata.title = title;
                session.snapshot = Arc::new(snapshot);
            }
        }
        self.rename_input.update(cx, TextInput::clear);
        cx.notify();
    }

    fn cancel_rename(&mut self, cx: &mut Context<Self>) {
        self.renaming_session = None;
        self.rename_input.update(cx, TextInput::clear);
        cx.notify();
    }

    fn delete_session(&mut self, app_session_id: String, cx: &mut Context<Self>) {
        self.session_menu = None;
        self.action_error = None;
        let _ = self
            .commands
            .send(ServiceCommand::DeleteSession { app_session_id });
        cx.notify();
    }

    /// Context menu for a session, anchored where the user right-clicked.
    fn session_context_menu(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let menu = self.session_menu.as_ref()?;
        let rename_id = menu.id.clone();
        let rename_title = menu.title.clone();
        let delete_id = menu.id.clone();
        let label = menu.title.clone();
        Some(
            div()
                .id("session-menu")
                .accessibility_id("session-menu")
                .role(Role::Menu)
                .aria_label(format!("Actions for {label}"))
                .absolute()
                .left(menu.position.x)
                .top(menu.position.y)
                .w(px(200.0))
                .flex()
                .flex_col()
                .p_1()
                .rounded_lg()
                .bg(rgb(ELEVATED))
                .border_1()
                .border_color(rgb(BORDER))
                .shadow_lg()
                .child(
                    div()
                        .id("session-menu-rename")
                        .debug_selector(|| "session-menu-rename".to_owned())
                        .accessibility_id("session-menu-rename")
                        .role(Role::MenuItem)
                        .aria_label("Rename session")
                        .focusable()
                        .tab_stop(true)
                        .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .text_sm()
                        .text_color(rgb(PRIMARY))
                        .child("Rename")
                        .hover(|style| style.bg(rgb(SUBTLE)).cursor_pointer())
                        .on_click(cx.listener(move |view, _, window, cx| {
                            view.begin_rename(rename_id.clone(), rename_title.clone(), window, cx);
                        })),
                )
                .child(
                    div()
                        .id("session-menu-delete")
                        .debug_selector(|| "session-menu-delete".to_owned())
                        .accessibility_id("session-menu-delete")
                        .role(Role::MenuItem)
                        .aria_label("Delete session")
                        .focusable()
                        .tab_stop(true)
                        .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .text_sm()
                        .text_color(rgb(RED))
                        .child("Delete session")
                        .hover(|style| style.bg(rgb(SUBTLE)).cursor_pointer())
                        .on_click(cx.listener(move |view, _, _, cx| {
                            view.delete_session(delete_id.clone(), cx);
                        })),
                ),
        )
    }

    /// Rename dialog for the session chosen from the context menu.
    fn rename_dialog(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        self.renaming_session.as_ref()?;
        Some(
            div()
                .id("rename-dialog")
                .accessibility_id("rename-dialog")
                .role(Role::Dialog)
                .aria_label("Rename session")
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(gpui::rgba(0x0000_00a8))
                .child(
                    div()
                        .id("rename-panel")
                        .w(px(460.0))
                        .flex()
                        .flex_col()
                        .gap_3()
                        .p_5()
                        .rounded_lg()
                        .bg(rgb(PANEL))
                        .border_1()
                        .border_color(rgb(BORDER))
                        .shadow_lg()
                        .child(
                            div()
                                .id("rename-heading")
                                .role(Role::Heading)
                                .aria_level(2)
                                .aria_label("Rename session")
                                .text_xl()
                                .font_weight(gpui::FontWeight::BOLD)
                                .child("Rename session"),
                        )
                        .child(
                            div()
                                .border_1()
                                .border_color(rgb(BORDER))
                                .rounded_md()
                                .child(self.rename_input.clone()),
                        )
                        .child(
                            div()
                                .flex()
                                .justify_end()
                                .gap_2()
                                .child(
                                    div()
                                        .id("rename-cancel")
                                        .accessibility_id("rename-cancel")
                                        .role(Role::Button)
                                        .aria_label("Cancel rename")
                                        .focusable()
                                        .tab_stop(true)
                                        .focus_visible(|style| {
                                            style.border_1().border_color(rgb(BLUE))
                                        })
                                        .px_4()
                                        .py_2()
                                        .rounded_md()
                                        .border_1()
                                        .border_color(rgb(BORDER))
                                        .text_color(rgb(MUTED))
                                        .child("Cancel")
                                        .hover(|style| style.bg(rgb(ELEVATED)).cursor_pointer())
                                        .on_click(cx.listener(|view, _, _, cx| {
                                            view.cancel_rename(cx);
                                        })),
                                )
                                .child(
                                    div()
                                        .id("rename-confirm")
                                        .accessibility_id("rename-confirm")
                                        .role(Role::Button)
                                        .aria_label("Confirm rename")
                                        .focusable()
                                        .tab_stop(true)
                                        .focus_visible(|style| {
                                            style.border_1().border_color(rgb(BLUE))
                                        })
                                        .px_4()
                                        .py_2()
                                        .rounded_md()
                                        .bg(rgb(GREEN))
                                        .text_color(rgb(BACKGROUND))
                                        .child("Rename")
                                        .hover(|style| style.opacity(0.85).cursor_pointer())
                                        .on_click(cx.listener(|view, _, _, cx| {
                                            let title = view.rename_input.read(cx).value();
                                            view.commit_rename(&title, cx);
                                        })),
                                ),
                        ),
                ),
        )
    }

    fn new_session(&mut self, cx: &mut Context<Self>) {
        self.open_control_menu = None;
        self.selected_session = None;
        let _ = self.commands.send(ServiceCommand::Select {
            app_session_id: None,
        });
        self.action_error = None;
        self.composer.update(cx, TextInput::clear);
        cx.notify();
    }

    fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.sidebar_open = !self.sidebar_open;
        cx.notify();
    }

    fn submit_composer(&mut self, cx: &mut Context<Self>) {
        let prompt = self.composer.read(cx).value();
        let prompt = prompt.trim();
        if !prompt.is_empty() {
            self.submit_prompt(prompt.to_owned());
            cx.notify();
        }
    }

    fn toggle_control_menu(&mut self, menu: ControlMenu) {
        self.open_control_menu = toggled_menu(self.open_control_menu, menu);
    }

    fn dismiss_control_menu(&mut self, cx: &mut Context<Self>) {
        if self.open_control_menu.take().is_some() {
            cx.notify();
        }
    }

    fn choose_control(&mut self, menu: ControlMenu, value: String, cx: &mut Context<Self>) {
        match menu {
            ControlMenu::Project => {
                self.open_control_menu = None;
                if value == ADD_PROJECT_OPTION {
                    self.add_project(cx);
                } else if value == CHAT_OPTION {
                    self.new_chat(cx);
                } else {
                    self.select_project(&value, cx);
                }
                return;
            }
            ControlMenu::Location => {
                self.draft_location = SessionLocation::from_str_or_default(&value);
            }
            ControlMenu::Mode => {
                value.clone_into(&mut self.draft_mode);
                if let Some(id) = self.selected_session.clone() {
                    let _ = self.commands.send(ServiceCommand::SetMode {
                        app_session_id: id,
                        mode: value,
                    });
                }
            }
            ControlMenu::Model => {
                let supported_efforts = self.supported_reasoning_efforts(&value);
                self.draft_model = Some(value.clone());
                let reasoning_effort = if supported_efforts.is_empty() {
                    None
                } else {
                    if !supported_efforts.contains(&self.draft_effort) {
                        self.draft_effort.clone_from(
                            supported_efforts
                                .iter()
                                .find(|effort| effort.as_str() == "medium")
                                .unwrap_or(&supported_efforts[0]),
                        );
                    }
                    Some(self.draft_effort.clone())
                };
                self.draft_context_tier = default_context_tier(&self.context_windows(&value));
                let context_tier = self.selectable_context_tier();
                if let Some(id) = self.selected_session.clone() {
                    let _ = self.commands.send(ServiceCommand::SetModel {
                        app_session_id: id,
                        model: value,
                        reasoning_effort,
                        context_tier,
                    });
                }
            }
            ControlMenu::Effort => {
                value.clone_into(&mut self.draft_effort);
                if let Some(id) = self.selected_session.clone() {
                    let _ = self.commands.send(ServiceCommand::SetReasoningEffort {
                        app_session_id: id,
                        effort: value,
                    });
                }
            }
            ControlMenu::Context => {
                self.draft_context_tier = Some(value.clone());
                if let Some(id) = self.selected_session.clone() {
                    let _ = self.commands.send(ServiceCommand::SetContextTier {
                        app_session_id: id,
                        tier: value,
                    });
                }
            }
        }
        self.open_control_menu = None;
    }

    fn provider_status(&self) -> (String, u32) {
        match &self.startup {
            StartupState::Starting => ("Starting Copilot...".to_owned(), AMBER),
            StartupState::Ready(compatibility) => (
                format!(
                    "Connected · protocol {} · pid {}",
                    compatibility.negotiated_protocol_version,
                    compatibility
                        .process_id
                        .map_or_else(|| "external".to_owned(), |pid| pid.to_string())
                ),
                GREEN,
            ),
            StartupState::Failed(error) => (error.clone(), RED),
        }
    }

    fn model_options(&self) -> Vec<(String, String, String)> {
        self.selected()
            .map(|session| &session.snapshot.controls.available_models)
            .filter(|models| !models.is_empty())
            .or(match &self.startup {
                StartupState::Ready(compatibility) => Some(&compatibility.available_models),
                StartupState::Starting | StartupState::Failed(_) => None,
            })
            .into_iter()
            .flatten()
            .map(|model| (model.id.clone(), model.name.clone(), String::new()))
            .collect()
    }

    /// Chat, the configured projects, and an entry that opens the folder
    /// picker. Chat leads because it needs no configuration.
    fn project_options(&self) -> Vec<(String, String, String)> {
        let mut options = vec![(
            CHAT_OPTION.to_owned(),
            "Chat".to_owned(),
            "A session with no repository".to_owned(),
        )];
        options.extend(self.projects.iter().map(|project| {
            let missing = !Path::new(&project.path).is_dir();
            let description = if missing {
                format!("{} (folder is missing)", project.path)
            } else {
                project.path.clone()
            };
            (project.path.clone(), project.name.clone(), description)
        }));
        options.push((
            ADD_PROJECT_OPTION.to_owned(),
            "Add project…".to_owned(),
            "Choose a folder on disk".to_owned(),
        ));
        options
    }

    fn mode_options(&self) -> Vec<(String, String, String)> {
        let modes = match &self.startup {
            StartupState::Ready(compatibility) => &compatibility.available_modes,
            StartupState::Starting | StartupState::Failed(_) => return Vec::new(),
        };
        modes
            .iter()
            .map(|mode| {
                let description = match mode.as_str() {
                    "interactive" => "Step-by-step collaboration",
                    "plan" => "Plan first, execute when ready",
                    "autopilot" => "End-to-end execution",
                    _ => "Copilot agent mode",
                };
                (mode.clone(), title_case(mode), description.to_owned())
            })
            .collect()
    }

    fn supported_reasoning_efforts(&self, model_id: &str) -> Vec<String> {
        // The per-session catalog can list a model without its reasoning
        // efforts. Treat that as missing information and fall back to the
        // app-level catalog, otherwise the thinking-level pill disappears once
        // a session is selected even though the model supports it.
        self.model_entry(model_id)
            .map(|model| model.supported_reasoning_efforts.clone())
            .filter(|efforts| !efforts.is_empty())
            .unwrap_or_default()
    }

    /// Catalog entry describing what a model *can* do.
    ///
    /// The application catalog is authoritative for capabilities. The
    /// per-session catalog is a collapsed view of the session's current state:
    /// it reports no reasoning efforts at all and folds the context tiers into
    /// a single `default` entry holding the active window. Preferring it made
    /// the thinking-level control vanish and the context-length control
    /// degrade to static text as soon as a session was selected. The session
    /// catalog is still used when the app catalog does not know the model.
    fn model_entry(&self, model_id: &str) -> Option<&app_model::ModelOption> {
        let app = match &self.startup {
            StartupState::Ready(compatibility) => compatibility
                .available_models
                .iter()
                .find(|model| model.id == model_id),
            StartupState::Starting | StartupState::Failed(_) => None,
        };
        app.or_else(|| {
            self.selected().and_then(|session| {
                session
                    .snapshot
                    .controls
                    .available_models
                    .iter()
                    .find(|model| model.id == model_id)
            })
        })
    }

    fn effort_options(&self) -> Vec<(String, String, String)> {
        let model_id = self.draft_model.as_deref().unwrap_or("gpt-5.6-sol");
        self.supported_reasoning_efforts(model_id)
            .into_iter()
            .map(|effort| {
                let description = match effort.as_str() {
                    "low" => "Faster responses",
                    "medium" => "Balanced reasoning",
                    "high" => "Deeper reasoning",
                    "xhigh" => "Most thorough reasoning",
                    _ => "Provider-supported reasoning level",
                };
                (
                    effort.clone(),
                    effort_label(&effort),
                    description.to_owned(),
                )
            })
            .collect()
    }

    fn context_windows(&self, model_id: &str) -> Vec<ContextWindowOption> {
        // Resolved through `model_entry` so a per-session catalog entry that
        // carries no capability detail falls back to the app catalog. Without
        // that, selecting a session silently drops the context-length control
        // the same way it dropped the thinking-level control.
        self.model_entry(model_id)
            .map_or_else(Vec::new, |model| model.context_windows.clone())
    }

    /// The model the composer will actually submit with, which falls back to
    /// the catalog's auto entry while no model has been picked explicitly.
    fn effective_model(&self) -> Option<String> {
        self.draft_model.clone().or_else(|| {
            self.model_options()
                .into_iter()
                .find_map(|(id, label, _)| label.eq_ignore_ascii_case("auto").then_some(id))
        })
    }

    fn draft_context_windows(&self) -> Vec<ContextWindowOption> {
        self.effective_model()
            .map_or_else(Vec::new, |model| self.context_windows(&model))
    }

    /// The tier to submit with a request, which is only meaningful when the
    /// model actually offers a choice between context windows.
    fn selectable_context_tier(&self) -> Option<String> {
        let windows = self.draft_context_windows();
        if windows.len() < 2 {
            return None;
        }
        self.draft_context_tier
            .clone()
            .or_else(|| default_context_tier(&windows))
    }

    fn context_options(&self) -> Vec<(String, String, String)> {
        self.draft_context_windows()
            .into_iter()
            .map(|window| {
                let description = match window.tier.as_str() {
                    "default" => "Standard context window",
                    "long_context" => "Extended context window",
                    _ => "Provider-supported context window",
                };
                (
                    window.tier.clone(),
                    context_window_label(&window),
                    description.to_owned(),
                )
            })
            .collect()
    }

    fn draft_context_label(&self) -> Option<String> {
        let windows = self.draft_context_windows();
        if windows.len() < 2 {
            return windows.first().map(context_window_label);
        }
        let selected = self
            .draft_context_tier
            .clone()
            .or_else(|| default_context_tier(&windows))?;
        windows
            .iter()
            .find(|window| window.tier == selected)
            .map(context_window_label)
    }

    /// Renders a context-length selector when the model offers more than one
    /// context window, and a plain readout when it offers exactly one.
    fn context_control(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let label = self.draft_context_label()?;
        if self.draft_context_windows().len() > 1 {
            Some(
                control_pill(
                    "context",
                    label,
                    ControlMenu::Context,
                    self.open_control_menu == Some(ControlMenu::Context),
                    cx,
                )
                .into_any_element(),
            )
        } else {
            Some(context_readout(label).into_any_element())
        }
    }

    fn draft_model_label(&self) -> String {
        let Some(selected) = self.draft_model.as_deref() else {
            return "Auto".to_owned();
        };
        self.model_options()
            .into_iter()
            .find_map(|(id, label, _)| (id == selected).then_some(label))
            .unwrap_or_else(|| selected.to_owned())
    }

    #[allow(clippy::too_many_lines)]
    fn control_menu(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let menu = self.open_control_menu?;
        let (title, selected, options) = match menu {
            ControlMenu::Project => (
                "Project",
                if self.targets_chat() {
                    CHAT_OPTION.to_owned()
                } else {
                    self.selected_project.to_string_lossy().into_owned()
                },
                self.project_options(),
            ),
            ControlMenu::Location => (
                "Where to run this session",
                self.draft_location.as_str().to_owned(),
                [
                    SessionLocation::NewWorktree,
                    SessionLocation::LocalRepository,
                ]
                .into_iter()
                .map(|location| {
                    (
                        location.as_str().to_owned(),
                        location.label().to_owned(),
                        location.description().to_owned(),
                    )
                })
                .collect(),
            ),
            ControlMenu::Mode => ("Mode", self.draft_mode.clone(), self.mode_options()),
            ControlMenu::Model => {
                let options = self.model_options();
                let selected = self
                    .draft_model
                    .clone()
                    .or_else(|| {
                        options.iter().find_map(|(id, label, _)| {
                            (label.eq_ignore_ascii_case("auto")).then(|| id.clone())
                        })
                    })
                    .unwrap_or_default();
                ("Model", selected, options)
            }
            ControlMenu::Effort => (
                "Reasoning effort",
                self.draft_effort.clone(),
                self.effort_options(),
            ),
            ControlMenu::Context => {
                let options = self.context_options();
                let selected = self
                    .draft_context_tier
                    .clone()
                    .or_else(|| options.first().map(|(tier, _, _)| tier.clone()))
                    .unwrap_or_default();
                ("Context length", selected, options)
            }
        };
        let width = if menu == ControlMenu::Model {
            px(340.0)
        } else {
            px(260.0)
        };
        Some(
            div()
                .id("composer-control-menu")
                .accessibility_id("composer-control-menu")
                .role(Role::ListBox)
                .aria_label(title)
                .w(width)
                .max_h(px(460.0))
                .overflow_y_scroll()
                .p_2()
                .rounded_lg()
                .border_1()
                .border_color(rgb(BORDER))
                .bg(rgb(PANEL))
                .shadow_lg()
                .child(
                    div()
                        .px_2()
                        .py_1()
                        .text_sm()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(rgb(MUTED))
                        .child(title),
                )
                .children(options.into_iter().enumerate().map(
                    |(index, (value, label, description))| {
                        let is_selected = value == selected;
                        let option_value = value.clone();
                        let has_description = !description.is_empty();
                        let accessible_label = label.clone();
                        let accessible_description = description.clone();
                        div()
                            .id(("control-option", index))
                            .accessibility_id(format!("{}-option-{value}", control_menu_id(menu)))
                            .role(Role::ListBoxOption)
                            .aria_label(accessible_label)
                            .aria_selected(is_selected)
                            .when(has_description, |option| {
                                option.aria_description(accessible_description)
                            })
                            .focusable()
                            .tab_stop(true)
                            .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                            .flex()
                            .items_center()
                            .gap_2()
                            .px_2()
                            .py_2()
                            .rounded_md()
                            .bg(if is_selected {
                                rgb(ELEVATED)
                            } else {
                                rgb(PANEL)
                            })
                            .hover(|style| style.bg(rgb(ELEVATED)).cursor_pointer())
                            .on_click(cx.listener(move |view, _, _, cx| {
                                view.choose_control(menu, option_value.clone(), cx);
                                cx.notify();
                            }))
                            .child(
                                div()
                                    .w(px(16.0))
                                    .text_color(rgb(MUTED))
                                    .child(if is_selected { "✓" } else { "" }),
                            )
                            .child(div().flex().flex_col().min_w_0().child(label).when(
                                has_description,
                                |content| {
                                    content.child(
                                        div().text_xs().text_color(rgb(MUTED)).child(description),
                                    )
                                },
                            ))
                    },
                )),
        )
    }

    #[allow(clippy::too_many_lines)]
    fn sidebar(&self, compact: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let selected_path = self.selected_project.to_string_lossy();
        let sessions = self
            .sessions
            .iter()
            .filter(|session| !session.snapshot.metadata.is_chat())
            .filter(|session| session.snapshot.metadata.project_key() == selected_path)
            .map(|session| {
                let id = session.handle.id().to_owned();
                let accessible_id = id.clone();
                let label = session.snapshot.metadata.title.clone();
                let menu_id = id.clone();
                let menu_label = label.clone();
                let selected = self.selected_session.as_deref() == Some(id.as_str());
                div()
                    .id(SharedString::from(format!("session-{id}")))
                    .debug_selector(|| "session-row".to_owned())
                    .accessibility_id(accessible_id)
                    .role(Role::ListItem)
                    .aria_label(label)
                    .aria_selected(selected)
                    .focusable()
                    .tab_stop(true)
                    .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                    .flex()
                    .items_center()
                    .gap_2()
                    .ml_5()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .bg(if selected {
                        rgb(ELEVATED)
                    } else {
                        rgb(SIDEBAR)
                    })
                    .hover(|style| style.bg(rgb(ELEVATED)).cursor_pointer())
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.select_session(id.clone(), cx);
                    }))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |view, event: &gpui::MouseDownEvent, _, cx| {
                            view.open_session_menu(
                                menu_id.clone(),
                                menu_label.clone(),
                                event.position,
                                cx,
                            );
                        }),
                    )
                    .child(
                        div()
                            .w(px(7.0))
                            .h(px(7.0))
                            .rounded_full()
                            .bg(status_color(session.snapshot.status)),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .text_sm()
                            .text_color(rgb(PRIMARY))
                            .overflow_hidden()
                            .child(session.snapshot.metadata.title.clone()),
                    )
            });
        let chats = self
            .sessions
            .iter()
            .filter(|session| session.snapshot.metadata.is_chat())
            .map(|session| {
                let id = session.handle.id().to_owned();
                let label = session.snapshot.metadata.title.clone();
                let menu_id = id.clone();
                let menu_label = label.clone();
                let selected = self.selected_session.as_deref() == Some(id.as_str());
                div()
                    .id(SharedString::from(format!("chat-{id}")))
                    .debug_selector(|| "chat-row".to_owned())
                    .accessibility_id(id.clone())
                    .role(Role::ListItem)
                    .aria_label(label.clone())
                    .aria_selected(selected)
                    .focusable()
                    .tab_stop(true)
                    .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                    .flex()
                    .items_center()
                    .gap_2()
                    .ml_5()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .bg(if selected {
                        rgb(ELEVATED)
                    } else {
                        rgb(SIDEBAR)
                    })
                    .hover(|style| style.bg(rgb(ELEVATED)).cursor_pointer())
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.select_session(id.clone(), cx);
                    }))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |view, event: &gpui::MouseDownEvent, _, cx| {
                            view.open_session_menu(
                                menu_id.clone(),
                                menu_label.clone(),
                                event.position,
                                cx,
                            );
                        }),
                    )
                    .child(
                        div()
                            .w(px(7.0))
                            .h(px(7.0))
                            .rounded_full()
                            .bg(status_color(session.snapshot.status)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_sm()
                            .text_color(rgb(PRIMARY))
                            .overflow_hidden()
                            .child(label),
                    )
            });
        let projects = self.projects.iter().map(|project| {
            let path = project.path.clone();
            let project_id = project.id.clone();
            let selected = project.path == selected_path;
            let label = project.name.clone();
            let missing = !Path::new(&project.path).is_dir();
            div()
                .id(SharedString::from(format!("project-{path}")))
                .accessibility_id(path.clone())
                .role(Role::ListItem)
                .aria_label(if missing {
                    format!("{label} (folder is missing)")
                } else {
                    label.clone()
                })
                .aria_selected(selected)
                .focusable()
                .tab_stop(true)
                .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                .flex()
                .items_center()
                .gap_2()
                .px_3()
                .py_2()
                .rounded_md()
                .text_sm()
                .text_color(if selected { rgb(PRIMARY) } else { rgb(MUTED) })
                .bg(rgb(SIDEBAR))
                .child(div().text_color(rgb(MUTED)).child("▱"))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .text_color(if missing { rgb(AMBER) } else { rgb(PRIMARY) })
                        .child(project.name.clone()),
                )
                .child(
                    div()
                        .id(SharedString::from(format!("remove-project-{project_id}")))
                        .accessibility_id(format!("remove-project-{project_id}"))
                        .role(Role::Button)
                        .aria_label(format!("Remove {label}"))
                        .focusable()
                        .tab_stop(true)
                        .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                        .px_1()
                        .text_xs()
                        .text_color(rgb(MUTED))
                        .child("✕")
                        .hover(|style| style.text_color(rgb(RED)).cursor_pointer())
                        .on_click(cx.listener(move |view, _, _, cx| {
                            view.remove_project(project_id.clone(), cx);
                        })),
                )
                .hover(|style| style.bg(rgb(ELEVATED)).cursor_pointer())
                .on_click(cx.listener(move |view, _, _, cx| {
                    view.select_project(&path, cx);
                }))
        });
        div()
            .id("sidebar")
            .accessibility_id("sidebar")
            .role(Role::Navigation)
            .aria_label("Projects and sessions")
            .flex()
            .flex_col()
            .w(if compact { px(300.0) } else { px(280.0) })
            .h_full()
            .bg(rgb(SIDEBAR))
            .border_r_1()
            .border_color(rgb(BORDER))
            .child(
                div()
                    .id("sidebar-titlebar")
                    .h(px(56.0))
                    .flex()
                    .items_center()
                    .pl_3()
                    .pr_3()
                    .gap_3()
                    .child(
                        div()
                            .id("sidebar-toggle")
                            .accessibility_id("sidebar-toggle")
                            .role(Role::Button)
                            .aria_label("Collapse sidebar")
                            .aria_expanded(true)
                            .focusable()
                            .tab_stop(true)
                            .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                            .w(px(24.0))
                            .h(px(24.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_md()
                            .text_color(rgb(MUTED))
                            .child("▯")
                            .hover(|style| style.bg(rgb(ELEVATED)).cursor_pointer())
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.toggle_sidebar(cx);
                            })),
                    )
                    .child(div().flex_1())
                    .child(div().text_color(rgb(MUTED)).child("<"))
                    .child(div().text_color(rgb(MUTED)).child(">")),
            )
            .child(
                div()
                    .id("primary-destinations")
                    .role(Role::Navigation)
                    .aria_label("Primary")
                    .flex()
                    .flex_col()
                    .px_2()
                    .gap_1()
                    .child(
                        div()
                            .id("destination-home")
                            .accessibility_id("destination-home")
                            .role(Role::Button)
                            .aria_label("Home")
                            .aria_selected(self.selected_session.is_none())
                            .focusable()
                            .tab_stop(true)
                            .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                            .flex()
                            .items_center()
                            .gap_3()
                            .px_3()
                            .py_2()
                            .rounded_md()
                            .bg(if self.selected_session.is_none() {
                                rgb(ELEVATED)
                            } else {
                                rgb(SIDEBAR)
                            })
                            .child(div().text_color(rgb(MUTED)).child("⌂"))
                            .child("Home")
                            .hover(|style| style.bg(rgb(ELEVATED)).cursor_pointer())
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.new_session(cx);
                            })),
                    )
                    .child(disabled_destination("destination-my-work", "☷", "My work"))
                    .child(disabled_destination(
                        "destination-automations",
                        "□",
                        "Automations",
                    ))
                    .child(disabled_destination("destination-search", "⌕", "Search")),
            )
            .child(
                div()
                    .mt_5()
                    .flex()
                    .items_center()
                    .px_4()
                    .text_sm()
                    .text_color(rgb(MUTED))
                    .child("Sessions")
                    .child(div().flex_1())
                    .child(div().id("session-grouping").text_xs().child("By project"))
                    .child(
                        div()
                            .id("new-session")
                            .accessibility_id("new-session")
                            .role(Role::Button)
                            .aria_label("New session")
                            .focusable()
                            .tab_stop(true)
                            .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                            .ml_3()
                            .w(px(24.0))
                            .h(px(24.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_md()
                            .text_lg()
                            .child("+")
                            .hover(|style| style.bg(rgb(ELEVATED)).cursor_pointer())
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.new_session(cx);
                            })),
                    ),
            )
            .child(
                div()
                    .id("session-list")
                    .role(Role::List)
                    .aria_label("Sessions")
                    .flex()
                    .flex_col()
                    .px_2()
                    .mt_2()
                    .gap_1()
                    .child(
                        div()
                            .id("chats-home")
                            .accessibility_id("chats-home")
                            .role(Role::Button)
                            .aria_label("Chats")
                            .aria_selected(self.selected_session.is_none())
                            .focusable()
                            .tab_stop(true)
                            .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                            .group("chats-row")
                            .flex()
                            .items_center()
                            .gap_3()
                            .px_3()
                            .py_2()
                            .rounded_md()
                            .text_color(rgb(MUTED))
                            .child("◯")
                            .child(div().flex_1().child("Chats"))
                            .child(
                                div()
                                    .id("new-chat")
                                    .debug_selector(|| "new-chat".to_owned())
                                    .accessibility_id("new-chat")
                                    .role(Role::Button)
                                    .aria_label("New chat")
                                    .focusable()
                                    .tab_stop(true)
                                    .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                                    .w(px(20.0))
                                    .h(px(20.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded_md()
                                    .text_color(rgb(MUTED))
                                    // Revealed on hover of the row, matching
                                    // how the app surfaces this affordance.
                                    .opacity(0.0)
                                    .group_hover("chats-row", |style| style.opacity(1.0))
                                    .child("+")
                                    .hover(|style| style.bg(rgb(ELEVATED)).text_color(rgb(PRIMARY)))
                                    .on_click(cx.listener(|view, _, _, cx| {
                                        view.new_chat(cx);
                                    })),
                            )
                            .hover(|style| style.bg(rgb(ELEVATED)).cursor_pointer())
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.new_chat(cx);
                            })),
                    )
                    .children(chats)
                    .children(projects)
                    .children(sessions)
                    .when(self.projects.is_empty(), |list| {
                        list.child(
                            div()
                                .id("no-projects")
                                .role(Role::Status)
                                .aria_label("No projects configured")
                                .px_3()
                                .py_2()
                                .text_xs()
                                .text_color(rgb(MUTED))
                                .child("No projects yet. Use Add project below the composer."),
                        )
                    }),
            )
            .children(
                self.restore_failures
                    .iter()
                    .enumerate()
                    .map(|(index, failure)| {
                        div()
                            .id(("restore-failure", index))
                            .role(Role::Alert)
                            .aria_label(format!("Restore failed: {}", failure.error))
                            .text_xs()
                            .text_color(rgb(RED))
                            .child(format!("Restore failed: {}", failure.error))
                    }),
            )
            .child(div().flex_1())
            .child(
                div()
                    .id("sidebar-footer")
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_4()
                    .pb_4()
                    .text_sm()
                    .child(
                        div()
                            .w(px(24.0))
                            .h(px(24.0))
                            .rounded_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .bg(rgb(ELEVATED))
                            .text_xs()
                            .child("GC"),
                    )
                    .child(div().flex_1().child("Local workspace"))
                    .child(
                        div()
                            .id("settings-placeholder")
                            .text_color(rgb(MUTED))
                            .child("Settings — unavailable"),
                    ),
            )
    }

    /// A bounded, scrollable block of detail inside a tool entry.
    ///
    /// Commands, diffs, and output are frequently taller than any sensible
    /// entry. Clipping them hid the interesting part; scrolling keeps the
    /// entry compact while leaving the whole thing reachable.
    ///
    /// The block consumes its own wheel events so scrolling inside it does not
    /// also scroll the transcript behind it, and draws a thumb on hover
    /// because there is no platform scrollbar behind an overflow container.
    fn detail_block(&self, id: &str, content: String, max_height: f32) -> impl IntoElement {
        let handle = self
            .detail_scrolls
            .borrow_mut()
            .entry(id.to_owned())
            .or_default()
            .clone();

        // Thumb geometry from the handle: the visible fraction sets its
        // height, the scrolled fraction sets its position.
        let scrollable = f32::from(handle.max_offset().y);
        let content_height = max_height + scrollable;
        let visible_fraction = (max_height / content_height).clamp(0.05, 1.0);
        let scrolled_fraction = if scrollable > 0.0 {
            (-f32::from(handle.offset().y) / scrollable).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let thumb_height = (max_height * visible_fraction).max(24.0);
        let thumb_top = (max_height - thumb_height) * scrolled_fraction;
        let group = SharedString::from(format!("scroll-{id}"));
        let needs_thumb = scrollable > 1.0;

        div()
            .id(SharedString::from(format!("{id}-frame")))
            .group(group.clone())
            .relative()
            .mt_1()
            .w_full()
            .child(
                div()
                    .id(SharedString::from(id.to_owned()))
                    .debug_selector(|| "tool-detail".to_owned())
                    .track_scroll(&handle)
                    .max_h(px(max_height))
                    .w_full()
                    .overflow_y_scroll()
                    // Without this the transcript scrolls too, so reading a
                    // command's output dragged the whole conversation along.
                    .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .bg(rgb(PANEL))
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .child(content),
            )
            .when(needs_thumb, |frame| {
                frame.child(
                    div()
                        .absolute()
                        .top(px(thumb_top))
                        .right(px(2.0))
                        .w(px(6.0))
                        .h(px(thumb_height))
                        .rounded_full()
                        .bg(rgb(BORDER))
                        // Only shown while the pointer is over this block, so
                        // resting entries stay quiet.
                        .opacity(0.0)
                        .group_hover(group, |style| style.opacity(1.0)),
                )
            })
    }

    /// A tool call in the timeline, with its nested subagent work.
    ///
    /// This is what makes a session observable: without it the transcript
    /// shows what the agent said while the reads, searches, edits, and
    /// commands it actually ran stay invisible.
    #[allow(clippy::too_many_lines)]
    fn tool_entry(
        &self,
        invocation: &app_model::ToolInvocation,
        children: &[&app_model::ToolInvocation],
    ) -> impl IntoElement {
        let (status, status_color) = match invocation.state {
            app_model::InvocationState::Running => ("running", GREEN),
            app_model::InvocationState::Succeeded => ("done", MUTED),
            app_model::InvocationState::Failed => ("failed", RED),
        };
        let summary = invocation.summary_line();
        let detail = invocation.multiline_summary();
        let verb = invocation.verb();
        let label = format!("{verb} {summary}");
        let diff = invocation.diff().map(str::to_owned);
        let error = invocation.error_message.clone();
        // Command output is the tail, since the interesting part is the end.
        // The block scrolls, so it can hold considerably more than the
        // terminals panel preview.
        let output = (invocation.class == app_model::ToolClass::Shell
            && !invocation.output.is_empty())
        .then(|| tail_lines(&invocation.output, 400));
        let exit = invocation
            .exit_code
            .filter(|code| *code != 0)
            .map(|code| format!("exit {code}"));
        let nested: Vec<_> = children
            .iter()
            .map(|child| {
                let child_status = match child.state {
                    app_model::InvocationState::Running => GREEN,
                    app_model::InvocationState::Succeeded => MUTED,
                    app_model::InvocationState::Failed => RED,
                };
                div()
                    .id(SharedString::from(format!("nested-{}", child.call_id)))
                    .role(Role::ListItem)
                    .aria_label(format!("{} {}", child.verb(), child.summary()))
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .w(px(5.0))
                            .h(px(5.0))
                            .rounded_full()
                            .bg(rgb(child_status)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child(format!("{} {}", child.verb(), child.summary())),
                    )
            })
            .collect();

        div()
            .id(SharedString::from(format!("tool-{}", invocation.call_id)))
            .debug_selector(|| "tool-entry".to_owned())
            .accessibility_id(invocation.call_id.clone())
            .role(Role::ListItem)
            .aria_label(format!("{label} ({status})"))
            .flex()
            .w_full()
            .justify_start()
            .child(
                div()
                    .max_w(px(760.0))
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .bg(rgb(SUBTLE))
                    .border_1()
                    .border_color(rgb(
                        if invocation.state == app_model::InvocationState::Failed {
                            RED
                        } else {
                            BORDER
                        },
                    ))
                    .child(
                        div()
                            .flex()
                            // Top aligned: a multi-line target must not push
                            // the label to the vertical middle of the block.
                            .items_start()
                            .gap_2()
                            .child(
                                div()
                                    .mt(px(5.0))
                                    .w(px(6.0))
                                    .h(px(6.0))
                                    .rounded_full()
                                    .bg(rgb(status_color)),
                            )
                            .child(div().text_xs().text_color(rgb(BLUE)).child(verb.to_owned()))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .text_xs()
                                    .text_color(rgb(PRIMARY))
                                    .child(summary),
                            )
                            .when_some(exit, |row, exit| {
                                row.child(div().text_xs().text_color(rgb(RED)).child(exit))
                            }),
                    )
                    .when_some(error, |entry, error| {
                        entry.child(div().text_xs().text_color(rgb(RED)).child(error))
                    })
                    .when_some(detail, |entry, detail| {
                        entry.child(self.detail_block(
                            &format!("tool-detail-{}", invocation.call_id),
                            detail,
                            COMMAND_BLOCK_HEIGHT,
                        ))
                    })
                    .when_some(diff, |entry, diff| {
                        entry.child(self.detail_block(
                            &format!("tool-diff-{}", invocation.call_id),
                            diff,
                            ENTRY_DETAIL_BUDGET - COMMAND_BLOCK_HEIGHT,
                        ))
                    })
                    .when_some(output, |entry, output| {
                        entry.child(self.detail_block(
                            &format!("tool-output-{}", invocation.call_id),
                            output,
                            ENTRY_DETAIL_BUDGET - COMMAND_BLOCK_HEIGHT,
                        ))
                    })
                    .when(!nested.is_empty(), |entry| {
                        entry.child(
                            div()
                                .mt_1()
                                .pl_3()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .border_l_1()
                                .border_color(rgb(BORDER))
                                .children(nested),
                        )
                    }),
            )
    }

    /// One conversation message.
    fn transcript_message(message: &app_model::TranscriptMessage) -> impl IntoElement {
        let is_user = message.role == TranscriptRole::User;
        let speaker = if is_user { "You" } else { "Copilot" };
        div()
            .id(SharedString::from(format!("message-{}", message.id)))
            .accessibility_id(message.id.clone())
            .role(Role::ListItem)
            .aria_label(format!("{speaker}: {}", message.content))
            .flex()
            .w_full()
            .justify_end()
            .when(!is_user, gpui::Styled::justify_start)
            .child(
                div()
                    .max_w(px(760.0))
                    .p_3()
                    .rounded_lg()
                    .bg(if is_user { rgb(ELEVATED) } else { rgb(PANEL) })
                    .border_1()
                    .border_color(rgb(BORDER))
                    .child(
                        div()
                            .text_xs()
                            .text_color(if is_user { rgb(BLUE) } else { rgb(GREEN) })
                            .child(if is_user { "You" } else { "Copilot" }),
                    )
                    .child(
                        div()
                            .mt_2()
                            .text_color(rgb(PRIMARY))
                            .child(message.content.clone()),
                    )
                    .when(message.state == TranscriptState::Interrupted, |bubble| {
                        bubble.child(div().mt_1().text_xs().text_color(rgb(AMBER)).child(
                            "Interrupted — the model does not have this \
                                             in its context",
                        ))
                    })
                    .when(message.state == TranscriptState::Streaming, |bubble| {
                        bubble.child(
                            div()
                                .mt_1()
                                .text_xs()
                                .text_color(rgb(MUTED))
                                .child("Streaming..."),
                        )
                    }),
            )
    }

    fn transcript(&self) -> impl IntoElement {
        let Some(session) = self.selected() else {
            return div()
                .id("empty-session")
                .role(Role::Group)
                .aria_label("New session")
                .flex()
                .flex_1()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .w(px(640.0))
                        .flex()
                        .flex_col()
                        .gap_3()
                        .items_center()
                        .child(
                            div()
                                .id("empty-session-heading")
                                .role(Role::Heading)
                                .aria_level(2)
                                .aria_label("What should Copilot work on?")
                                .text_2xl()
                                .child("What should Copilot work on?"),
                        )
                        .child(
                            div()
                                .text_color(rgb(MUTED))
                                .child("Start a coding session in the current checkout."),
                        ),
                );
        };
        // The whole conversation is rendered now that the transcript scrolls;
        // capping it would put a wall part-way up the scrollback. Phase 6
        // replaces this with the virtualized list.
        let entries = session
            .snapshot
            .timeline()
            .into_iter()
            .map(|entry| match entry {
                app_model::TimelineEntry::Message(message) => {
                    Self::transcript_message(message).into_any_element()
                }
                app_model::TimelineEntry::Tool(invocation) => {
                    let children = session
                        .snapshot
                        .tool_activity
                        .children_of(&invocation.call_id);
                    self.tool_entry(invocation, &children).into_any_element()
                }
            })
            .collect::<Vec<_>>();
        div()
            .id("transcript")
            .debug_selector(|| "transcript".to_owned())
            .role(Role::List)
            .aria_label("Conversation")
            .track_scroll(&self.transcript_scroll)
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .p_5()
            .gap_3()
            .overflow_y_scroll()
            .children(entries)
    }

    /// Phase 3 inspector: changes, terminals, and capability state.
    ///
    /// Rendered beside the transcript so the edit-command-result-diff loop can
    /// be completed without leaving GCABB.
    fn side_panel(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let session = self.selected()?;
        let snapshot = session.snapshot.clone();
        let active = self.active_panel;
        let tabs = SessionPanel::ALL.map(|panel| {
            let selected = panel == active;
            div()
                .id(panel.id())
                .accessibility_id(panel.id())
                .role(Role::Tab)
                .aria_label(panel.label())
                .aria_selected(selected)
                .focusable()
                .tab_stop(true)
                .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                .px_3()
                .py_1()
                .text_xs()
                .rounded_md()
                .text_color(if selected { rgb(PRIMARY) } else { rgb(MUTED) })
                .when(selected, |tab| tab.bg(rgb(ELEVATED)))
                .child(panel.label())
                .hover(|style| style.bg(rgb(SUBTLE)).cursor_pointer())
                .on_click(cx.listener(move |view, _, _, cx| {
                    view.active_panel = panel;
                    cx.notify();
                }))
        });

        let body = match active {
            SessionPanel::Changes => self.changes_panel(&snapshot, cx).into_any_element(),
            SessionPanel::Terminals => Self::terminals_panel(&snapshot).into_any_element(),
            SessionPanel::Capabilities => Self::capabilities_panel(&snapshot).into_any_element(),
        };

        Some(
            div()
                .id("session-panel")
                .accessibility_id("session-panel")
                .role(Role::Group)
                .aria_label("Session inspector")
                .flex()
                .flex_col()
                .w(px(420.0))
                .min_h_0()
                .border_l_1()
                .border_color(rgb(BORDER))
                .bg(rgb(SIDEBAR))
                .child(
                    div()
                        .id("session-panel-tabs")
                        .role(Role::TabList)
                        .aria_label("Inspector sections")
                        .flex()
                        .gap_1()
                        .p_2()
                        .border_b_1()
                        .border_color(rgb(BORDER))
                        .children(tabs),
                )
                .child(
                    div()
                        .id("session-panel-body")
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_h_0()
                        .overflow_hidden()
                        .p_3()
                        .gap_2()
                        .child(body),
                ),
        )
    }

    #[allow(clippy::too_many_lines)]
    fn changes_panel(
        &self,
        snapshot: &SessionSnapshot,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let changes = &snapshot.changes;
        if let Some(error) = &changes.error {
            return div()
                .id("changes-error")
                .role(Role::Alert)
                .aria_label(error.clone())
                .text_sm()
                .text_color(rgb(RED))
                .child(error.clone())
                .into_any_element();
        }
        if changes.is_empty() {
            return div()
                .id("changes-empty")
                .role(Role::Status)
                .aria_label("No changes")
                .text_sm()
                .text_color(rgb(MUTED))
                .child(format!(
                    "No changes against {}.",
                    changes.base_label.as_deref().unwrap_or("base")
                ))
                .into_any_element();
        }

        let totals = changes.totals();
        let selected_path = self
            .selected_change
            .clone()
            .or_else(|| changes.files.first().map(|file| file.path.clone()));
        let rows = changes.files.iter().map(|file| {
            let path = file.path.clone();
            let is_selected = selected_path.as_deref() == Some(path.as_str());
            let label = format!(
                "{} {} +{} -{}",
                file.status.label(),
                path,
                file.stats.insertions,
                file.stats.deletions
            );
            div()
                .id(SharedString::from(format!("change-{path}")))
                .role(Role::ListItem)
                .aria_label(label)
                .aria_selected(is_selected)
                .focusable()
                .tab_stop(true)
                .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                .flex()
                .justify_between()
                .gap_2()
                .px_2()
                .py_1()
                .rounded_md()
                .when(is_selected, |row| row.bg(rgb(ELEVATED)))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_xs()
                        .text_color(rgb(PRIMARY))
                        .child(path.clone()),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(GREEN))
                        .child(format!("+{}", file.stats.insertions)),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(RED))
                        .child(format!("-{}", file.stats.deletions)),
                )
                .hover(|style| style.bg(rgb(SUBTLE)).cursor_pointer())
                .on_click(cx.listener(move |view, _, _, cx| {
                    view.selected_change = Some(path.clone());
                    cx.notify();
                }))
        });

        let diff = selected_path
            .as_deref()
            .and_then(|path| changes.file(path))
            .map(|file| {
                file.diff.clone().unwrap_or_else(|| {
                    file.diff_omitted_reason
                        .clone()
                        .unwrap_or_else(|| "No diff available.".to_owned())
                })
            })
            .unwrap_or_default();

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .gap_2()
            .child(
                div()
                    .id("changes-summary")
                    .role(Role::Status)
                    .aria_label(format!(
                        "{} files changed, {} insertions, {} deletions against {}",
                        changes.files.len(),
                        totals.insertions,
                        totals.deletions,
                        changes.base_label.as_deref().unwrap_or("base")
                    ))
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .child(format!(
                        "{} file(s) · +{} −{} · vs {}",
                        changes.files.len(),
                        totals.insertions,
                        totals.deletions,
                        changes.base_label.as_deref().unwrap_or("base")
                    )),
            )
            .child(
                div()
                    .id("changes-list")
                    .role(Role::List)
                    .aria_label("Changed files")
                    .flex()
                    .flex_col()
                    .gap_1()
                    .max_h(px(220.0))
                    .overflow_hidden()
                    .children(rows),
            )
            .child(
                div()
                    .id("changes-diff")
                    .role(Role::Group)
                    .aria_label("Unified diff")
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .p_2()
                    .rounded_md()
                    .bg(rgb(PANEL))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .text_xs()
                    .text_color(rgb(PRIMARY))
                    .child(diff),
            )
            .into_any_element()
    }

    fn terminals_panel(snapshot: &SessionSnapshot) -> impl IntoElement {
        let terminals = &snapshot.tool_activity.terminals;
        if terminals.is_empty() {
            return div()
                .id("terminals-empty")
                .role(Role::Status)
                .aria_label("No terminals")
                .text_sm()
                .text_color(rgb(MUTED))
                .child("No shell commands have run in this session.")
                .into_any_element();
        }
        let cards = terminals.iter().rev().take(12).map(|terminal| {
            let (state_label, state_color) = match terminal.state {
                app_model::TerminalState::Running => ("running", GREEN),
                app_model::TerminalState::Exited => ("exited", MUTED),
                app_model::TerminalState::Cancelled => ("cancelled", RED),
            };
            let exit = terminal
                .exit_code
                .map_or_else(String::new, |code| format!(" · exit {code}"));
            let command = terminal
                .command
                .clone()
                .unwrap_or_else(|| "(command unavailable)".to_owned());
            div()
                .id(SharedString::from(format!(
                    "terminal-{}",
                    terminal.shell_id
                )))
                .accessibility_id(terminal.shell_id.clone())
                .role(Role::Group)
                .aria_label(format!("Shell {} {state_label}", terminal.shell_id))
                .flex()
                .flex_col()
                .gap_1()
                .p_2()
                .rounded_md()
                .bg(rgb(PANEL))
                .border_1()
                .border_color(rgb(BORDER))
                .child(
                    div()
                        .flex()
                        .justify_between()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_xs()
                                .text_color(rgb(PRIMARY))
                                .child(command),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(state_color))
                                .child(format!("{state_label}{exit}")),
                        ),
                )
                .child(div().text_xs().text_color(rgb(MUTED)).child(format!(
                    "shell {} · {} call(s)",
                    terminal.shell_id,
                    terminal.tool_call_ids.len()
                )))
                .child(
                    div()
                        .max_h(px(160.0))
                        .overflow_hidden()
                        .text_xs()
                        .text_color(rgb(PRIMARY))
                        .child(terminal_tail(&terminal.output)),
                )
                .when(terminal.output_truncated, |card| {
                    card.child(
                        div()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child("Earlier output was trimmed."),
                    )
                })
        });
        div()
            .id("terminals-list")
            .role(Role::List)
            .aria_label("Terminals")
            .flex()
            .flex_col()
            .gap_2()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .children(cards)
            .into_any_element()
    }

    #[allow(clippy::too_many_lines)]
    fn capabilities_panel(snapshot: &SessionSnapshot) -> impl IntoElement {
        let report = &snapshot.capabilities;
        let failures = snapshot.tool_activity.failures();
        let rows = report.capabilities.iter().map(|capability| {
            let (label, color) = match capability.status {
                app_model::CapabilityStatus::Available => ("available", GREEN),
                app_model::CapabilityStatus::Unavailable => ("unavailable", RED),
                app_model::CapabilityStatus::NeedsAttention => ("needs attention", AMBER),
                app_model::CapabilityStatus::Unknown => ("unknown", MUTED),
            };
            div()
                .id(SharedString::from(format!(
                    "capability-{}",
                    capability.id.label().to_lowercase().replace(' ', "-")
                )))
                .role(Role::ListItem)
                .aria_label(format!(
                    "{}: {label}. {}",
                    capability.id.label(),
                    capability.detail
                ))
                .flex()
                .flex_col()
                .gap_1()
                .p_2()
                .rounded_md()
                .bg(rgb(PANEL))
                .border_1()
                .border_color(rgb(BORDER))
                .child(
                    div()
                        .flex()
                        .justify_between()
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(PRIMARY))
                                .child(capability.id.label()),
                        )
                        .child(div().text_xs().text_color(rgb(color)).child(label)),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(MUTED))
                        .child(capability.detail.clone()),
                )
        });

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .gap_2()
            .overflow_hidden()
            .child(
                div()
                    .id("capabilities-list")
                    .role(Role::List)
                    .aria_label("Capabilities")
                    .flex()
                    .flex_col()
                    .gap_2()
                    .children(rows),
            )
            .when(!failures.is_empty(), |panel| {
                let items = failures.into_iter().rev().take(6).map(|invocation| {
                    let message = invocation
                        .error_message
                        .clone()
                        .unwrap_or_else(|| "Tool failed without a message.".to_owned());
                    div()
                        .id(SharedString::from(format!(
                            "tool-failure-{}",
                            invocation.call_id
                        )))
                        .role(Role::ListItem)
                        .aria_label(format!("{} failed: {message}", invocation.tool_name))
                        .flex()
                        .flex_col()
                        .p_2()
                        .rounded_md()
                        .bg(rgb(PANEL))
                        .border_1()
                        .border_color(rgb(RED))
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(RED))
                                .child(invocation.tool_name.clone()),
                        )
                        .child(div().text_xs().text_color(rgb(MUTED)).child(message))
                });
                panel.child(
                    div()
                        .id("tool-failures")
                        .role(Role::List)
                        .aria_label("Recent tool failures")
                        .flex()
                        .flex_col()
                        .gap_2()
                        .children(items),
                )
            })
            .into_any_element()
    }

    #[allow(clippy::too_many_lines)]
    fn session_composer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mode = title_case(&self.draft_mode);
        let effort = effort_label(&self.draft_effort);
        let model = self.draft_model_label();
        let supports_reasoning = !self.effort_options().is_empty();
        let context_control = self.context_control(cx);
        let selected = self.selected();
        let running = selected.is_some_and(|session| {
            matches!(
                session.snapshot.status,
                SessionStatus::Running | SessionStatus::Starting
            )
        });
        let cancel = self.selected_session.clone();
        let disconnected =
            selected.is_some_and(|session| session.snapshot.status == SessionStatus::Disconnected);
        let close = (!disconnected)
            .then(|| self.selected_session.clone())
            .flatten();
        let resume = disconnected
            .then(|| self.selected_session.clone())
            .flatten();
        div()
            .id("composer")
            .accessibility_id("composer")
            .relative()
            .role(Role::Group)
            .aria_label("Message composer")
            .mx_auto()
            .mb_4()
            .w_full()
            .max_w(px(820.0))
            .flex()
            .flex_col()
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(BORDER))
            .rounded_lg()
            .shadow_lg()
            .child(self.composer.clone())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .pb_3()
                    .child(
                        div()
                            .id("attachments-placeholder")
                            .text_lg()
                            .text_color(rgb(MUTED))
                            .child("+"),
                    )
                    .child(control_pill(
                        "mode",
                        mode,
                        ControlMenu::Mode,
                        self.open_control_menu == Some(ControlMenu::Mode),
                        cx,
                    ))
                    .child(control_pill(
                        "model",
                        model,
                        ControlMenu::Model,
                        self.open_control_menu == Some(ControlMenu::Model),
                        cx,
                    ))
                    .when(supports_reasoning, |row| {
                        row.child(control_pill(
                            "effort",
                            effort,
                            ControlMenu::Effort,
                            self.open_control_menu == Some(ControlMenu::Effort),
                            cx,
                        ))
                    })
                    .children(context_control)
                    .child(div().flex_1())
                    .child(
                        div()
                            .id("submit-prompt")
                            .accessibility_id("submit-prompt")
                            .role(Role::Button)
                            .aria_label("Send message")
                            .focusable()
                            .tab_stop(true)
                            .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                            .w(px(32.0))
                            .h(px(32.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .bg(rgb(ELEVATED))
                            .text_color(rgb(MUTED))
                            .child("↑")
                            .hover(|style| {
                                style
                                    .bg(rgb(BORDER))
                                    .text_color(rgb(PRIMARY))
                                    .cursor_pointer()
                            })
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.submit_composer(cx);
                            })),
                    )
                    .when_some(cancel, |row, id| {
                        row.child(
                            div()
                                .id("cancel-session")
                                .px_3()
                                .py_1()
                                .rounded_md()
                                .border_1()
                                .border_color(if running { rgb(RED) } else { rgb(BORDER) })
                                .text_color(if running { rgb(RED) } else { rgb(MUTED) })
                                .child(if running { "Cancel" } else { "Idle" })
                                .when(running, |button| {
                                    button
                                        .accessibility_id("cancel-session")
                                        .role(Role::Button)
                                        .aria_label("Cancel current session")
                                        .focusable()
                                        .tab_stop(true)
                                        .focus_visible(|style| {
                                            style.border_1().border_color(rgb(BLUE))
                                        })
                                        .hover(|style| style.bg(rgb(ELEVATED)).cursor_pointer())
                                        .on_click(cx.listener(move |view, _, _, _| {
                                            let _ = view.commands.send(ServiceCommand::Cancel {
                                                app_session_id: id.clone(),
                                            });
                                        }))
                                }),
                        )
                    })
                    .when_some(close, |row, id| {
                        row.child(
                            div()
                                .id("close-session")
                                .accessibility_id("close-session")
                                .role(Role::Button)
                                .aria_label("Close session")
                                .focusable()
                                .tab_stop(true)
                                .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                                .px_3()
                                .py_1()
                                .rounded_md()
                                .text_color(rgb(MUTED))
                                .child("Close")
                                .hover(|style| style.bg(rgb(ELEVATED)).cursor_pointer())
                                .on_click(cx.listener(move |view, _, _, cx| {
                                    let _ = view.commands.send(ServiceCommand::Close {
                                        app_session_id: id.clone(),
                                    });
                                    cx.notify();
                                })),
                        )
                    })
                    .when(disconnected, |row| {
                        row.when_some(resume, |row, id| {
                            row.child(
                                div()
                                    .id("resume-session")
                                    .accessibility_id("resume-session")
                                    .role(Role::Button)
                                    .aria_label("Resume session")
                                    .focusable()
                                    .tab_stop(true)
                                    .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                                    .px_3()
                                    .py_1()
                                    .rounded_md()
                                    .bg(rgb(GREEN))
                                    .text_color(rgb(BACKGROUND))
                                    .child("Resume")
                                    .hover(|style| style.opacity(0.85).cursor_pointer())
                                    .on_click(cx.listener(move |view, _, _, _| {
                                        let _ = view.commands.send(ServiceCommand::Resume {
                                            app_session_id: id.clone(),
                                        });
                                    })),
                            )
                        })
                    }),
            )
    }

    #[allow(clippy::too_many_lines)]
    fn home_composer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let project_name = self.composer_project_label();
        let chat = self.targets_chat();
        let location_label = self.draft_location.label().to_owned();
        let branch = self.composer_branch_label();
        let mode = title_case(&self.draft_mode);
        let model = self.draft_model_label();
        let effort = effort_label(&self.draft_effort);
        let supports_reasoning = !self.effort_options().is_empty();
        let context_control = self.context_control(cx);

        div()
            .id("home-composer")
            .accessibility_id("home-composer")
            .role(Role::Group)
            .aria_label("Message composer")
            .relative()
            .w_full()
            .max_w(px(820.0))
            .flex()
            .flex_col()
            .rounded_lg()
            .bg(rgb(SUBTLE))
            .shadow_lg()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .min_h(px(108.0))
                    .bg(rgb(PANEL))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .rounded_lg()
                    .child(self.composer.clone())
                    .child(div().flex_1())
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .px_3()
                            .pb_3()
                            .child(
                                div()
                                    .id("home-attachments-placeholder")
                                    .w(px(28.0))
                                    .h(px(28.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded_md()
                                    .text_lg()
                                    .text_color(rgb(MUTED))
                                    .child("+"),
                            )
                            .child(control_pill(
                                "mode",
                                mode,
                                ControlMenu::Mode,
                                self.open_control_menu == Some(ControlMenu::Mode),
                                cx,
                            ))
                            .child(div().h(px(20.0)).border_l_1().border_color(rgb(BORDER)))
                            .child(control_pill(
                                "model",
                                model,
                                ControlMenu::Model,
                                self.open_control_menu == Some(ControlMenu::Model),
                                cx,
                            ))
                            .when(supports_reasoning, |row| {
                                row.child(control_pill(
                                    "effort",
                                    effort,
                                    ControlMenu::Effort,
                                    self.open_control_menu == Some(ControlMenu::Effort),
                                    cx,
                                ))
                            })
                            .children(context_control)
                            .child(div().flex_1())
                            .child(
                                div()
                                    .id("home-submit-prompt")
                                    .accessibility_id("home-submit-prompt")
                                    .role(Role::Button)
                                    .aria_label("Send message")
                                    .focusable()
                                    .tab_stop(true)
                                    .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                                    .w(px(32.0))
                                    .h(px(32.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded_full()
                                    .bg(rgb(ELEVATED))
                                    .text_color(rgb(MUTED))
                                    .child("↑")
                                    .hover(|style| {
                                        style
                                            .bg(rgb(BORDER))
                                            .text_color(rgb(PRIMARY))
                                            .cursor_pointer()
                                    })
                                    .on_click(cx.listener(|view, _, _, cx| {
                                        view.submit_composer(cx);
                                    })),
                            ),
                    ),
            )
            .child(
                div()
                    .id("checkout-context")
                    .flex()
                    .items_center()
                    .gap_4()
                    .h(px(48.0))
                    .px_4()
                    .min_w_0()
                    .overflow_hidden()
                    .text_sm()
                    .text_color(rgb(MUTED))
                    .child(
                        div()
                            .id("project-pill")
                            .accessibility_id("project-pill")
                            .role(Role::ComboBox)
                            .aria_label("Project")
                            .aria_value(project_name.clone())
                            .aria_expanded(self.open_control_menu == Some(ControlMenu::Project))
                            .focusable()
                            .tab_stop(true)
                            .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .child(format!("▱ {project_name}"))
                            .hover(|style| style.bg(rgb(ELEVATED)).cursor_pointer())
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.toggle_control_menu(ControlMenu::Project);
                                cx.notify();
                            })),
                    )
                    // A chat has no checkout, so the checkout details are
                    // replaced rather than shown as if they applied.
                    .when(chat, |strip| strip.child("↗ No repository"))
                    .when(!chat, |strip| {
                        strip
                            .child(
                                div()
                                    .id("location-pill")
                                    .debug_selector(|| "location-pill".to_owned())
                                    .accessibility_id("location-pill")
                                    .role(Role::ComboBox)
                                    .aria_label("Where to run this session")
                                    .aria_value(location_label.clone())
                                    .aria_expanded(
                                        self.open_control_menu == Some(ControlMenu::Location),
                                    )
                                    .focusable()
                                    .tab_stop(true)
                                    .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                                    .px_2()
                                    .py_1()
                                    .rounded_md()
                                    .child(format!("↗ {location_label}"))
                                    .hover(|style| style.bg(rgb(ELEVATED)).cursor_pointer())
                                    .on_click(cx.listener(|view, _, _, cx| {
                                        view.toggle_control_menu(ControlMenu::Location);
                                        cx.notify();
                                    })),
                            )
                            .child(format!("⌁ {branch}"))
                    })
                    .child(div().flex_1())
                    .child(
                        div()
                            .id("add-project")
                            .accessibility_id("add-project")
                            .role(Role::Button)
                            .aria_label("Add project folder")
                            .focusable()
                            .tab_stop(true)
                            .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .child("+ Add project")
                            .hover(|style| {
                                style
                                    .bg(rgb(ELEVATED))
                                    .text_color(rgb(PRIMARY))
                                    .cursor_pointer()
                            })
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.add_project(cx);
                            })),
                    ),
            )
    }

    #[allow(clippy::too_many_lines)]
    fn home(&self, compact: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let (provider_status, provider_color) = self.provider_status();

        div()
            .id("home")
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .when(compact, gpui::StatefulInteractiveElement::overflow_y_scroll)
            .when(!compact, gpui::Styled::overflow_hidden)
            .px(if compact { px(24.0) } else { px(40.0) })
            .pb_6()
            .child(
                div()
                    .id("provider-status")
                    .role(Role::Status)
                    .aria_label(provider_status.clone())
                    .absolute()
                    .top(px(20.0))
                    .right(px(24.0))
                    .text_xs()
                    .text_color(rgb(provider_color))
                    .child(provider_status),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .w_full()
                    .pt(if compact { px(92.0) } else { px(118.0) })
                    .child(
                        div()
                            .id("gcabb-mark")
                            .w(px(72.0))
                            .h(px(72.0))
                            .mb_10()
                            .rounded_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .bg(rgb(MUTED))
                            .text_color(rgb(BACKGROUND))
                            .text_xl()
                            .font_weight(gpui::FontWeight::BOLD)
                            .child("GC"),
                    )
                    .child(self.home_composer(cx)),
            )
    }

    #[allow(clippy::too_many_lines)]
    fn interaction_dialog(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let session = self.selected()?;
        let interaction = session.snapshot.pending_interactions.first()?.clone();
        let app_session_id = session.handle.id().to_owned();
        let interaction_id = interaction.id.clone();
        let approve = interaction_id.clone();
        let reject = interaction_id.clone();
        let approve_session = app_session_id.clone();
        let cancel_session = app_session_id.clone();
        let choices = interaction
            .choices
            .iter()
            .enumerate()
            .filter(|_| interaction.kind != InteractionKind::Permission)
            .map(|(index, choice)| {
                let choice = choice.clone();
                let kind = interaction.kind;
                let id = interaction_id.clone();
                let session_id = app_session_id.clone();
                div()
                    .id(("interaction-choice", index))
                    .accessibility_id(format!("interaction-choice-{index}"))
                    .role(Role::Button)
                    .aria_label(choice.clone())
                    .focusable()
                    .tab_stop(true)
                    .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(BORDER))
                    .child(choice.clone())
                    .hover(|style| style.bg(rgb(ELEVATED)).cursor_pointer())
                    .on_click(cx.listener(move |view, _, _, _| {
                        let _ = view.commands.send(ServiceCommand::Respond {
                            app_session_id: session_id.clone(),
                            interaction_id: id.clone(),
                            response: choice_response(kind, &choice),
                        });
                    }))
            });
        Some(
            div()
                .id("interaction-dialog")
                .accessibility_id("interaction-dialog")
                .role(Role::Dialog)
                .aria_label(interaction.title.clone())
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(gpui::rgba(0x0000_00a8))
                .child(
                    div()
                        .id("interaction-panel")
                        .w(px(560.0))
                        .flex()
                        .flex_col()
                        .gap_3()
                        .p_5()
                        .rounded_lg()
                        .bg(rgb(PANEL))
                        .border_1()
                        .border_color(rgb(BORDER))
                        .shadow_lg()
                        .child(
                            div()
                                .id("interaction-heading")
                                .role(Role::Heading)
                                .aria_level(2)
                                .aria_label(interaction.title.clone())
                                .text_xl()
                                .font_weight(gpui::FontWeight::BOLD)
                                .child(interaction.title),
                        )
                        .child(div().text_color(rgb(MUTED)).child(interaction.message))
                        .children(choices)
                        .when(interaction.allow_freeform, |dialog| {
                            dialog.child(
                                div()
                                    .border_1()
                                    .border_color(rgb(BORDER))
                                    .rounded_md()
                                    .child(self.interaction_input.clone()),
                            )
                        })
                        .when(interaction.kind == InteractionKind::Permission, |dialog| {
                            let session_id = app_session_id.clone();
                            dialog.child(
                                div()
                                    .flex()
                                    .justify_end()
                                    .gap_2()
                                    .child(action_button("Deny", RED, cx, move |view| {
                                        let _ = view.commands.send(ServiceCommand::Respond {
                                            app_session_id: session_id.clone(),
                                            interaction_id: reject.clone(),
                                            response: InteractionResponse::Reject {
                                                feedback: None,
                                            },
                                        });
                                    }))
                                    .child(action_button("Allow once", GREEN, cx, move |view| {
                                        let _ = view.commands.send(ServiceCommand::Respond {
                                            app_session_id: approve_session.clone(),
                                            interaction_id: approve.clone(),
                                            response: InteractionResponse::Approve,
                                        });
                                    })),
                            )
                        })
                        .when(interaction.kind != InteractionKind::Permission, |dialog| {
                            dialog.child(div().flex().justify_end().child(action_button(
                                "Cancel",
                                RED,
                                cx,
                                move |view| {
                                    let _ = view.commands.send(ServiceCommand::Respond {
                                        app_session_id: cancel_session.clone(),
                                        interaction_id: interaction_id.clone(),
                                        response: InteractionResponse::Cancel,
                                    });
                                },
                            )))
                        }),
                ),
        )
    }
}

impl Render for SessionMvpView {
    #[allow(clippy::too_many_lines)]
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.follow_transcript_tail();
        let (provider_status, provider_color) = self.provider_status();
        let compact = compact_layout(f32::from(window.viewport_size().width));
        let show_sidebar = self.sidebar_open;
        let content_left = if show_sidebar {
            if compact { 300.0 } else { 280.0 }
        } else {
            0.0
        };
        let control_menu_left = self.open_control_menu.map_or(0, control_menu_offset);
        let session_selected = self.selected_session.is_some();
        let title = self.selected().map_or_else(
            || "New session".to_owned(),
            |session| session.snapshot.metadata.title.clone(),
        );
        // The session's own worktree branch, not the repository default. The
        // changes view already resolved it, so no extra git call is needed.
        // A chat has no checkout, so it reports no repository instead of
        // inheriting an unrelated branch name.
        let chat = self
            .selected()
            .is_some_and(|session| session.snapshot.metadata.is_chat());
        let branch = if chat {
            "no repository".to_owned()
        } else {
            self.selected()
                .and_then(|session| session.snapshot.changes.branch.clone())
                .filter(|branch| !branch.is_empty())
                .unwrap_or_else(|| self.branch.clone())
        };
        let workspace = self.selected().map_or_else(
            || self.workspace_root.clone(),
            |session| PathBuf::from(&session.snapshot.metadata.project_path),
        );
        div()
            .id("gcabb")
            .accessibility_id("gcabb")
            .role(Role::Application)
            .aria_label("GCABB")
            .on_action(cx.listener(|_, _: &FocusNext, window, cx| {
                window.focus_next(cx);
            }))
            .on_action(cx.listener(|_, _: &FocusPrevious, window, cx| {
                window.focus_prev(cx);
            }))
            .on_action(cx.listener(|view, _: &DismissPopup, _, cx| {
                view.dismiss_control_menu(cx);
                view.dismiss_session_menu(cx);
                if view.renaming_session.is_some() {
                    view.cancel_rename(cx);
                }
            }))
            .relative()
            .flex()
            .size_full()
            .bg(rgb(BACKGROUND))
            .text_color(rgb(PRIMARY))
            .when(show_sidebar, |root| root.child(self.sidebar(compact, cx)))
            .child(
                div()
                    .id("main-content")
                    .relative()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .when(!show_sidebar, |main| {
                        main.child(
                            div()
                                .id("collapsed-titlebar")
                                .absolute()
                                .top_0()
                                .left_0()
                                .h(px(56.0))
                                .flex()
                                .items_center()
                                .pl_3()
                                .child(
                                    div()
                                        .id("sidebar-toggle")
                                        .accessibility_id("sidebar-toggle")
                                        .role(Role::Button)
                                        .aria_label("Expand sidebar")
                                        .aria_expanded(false)
                                        .focusable()
                                        .tab_stop(true)
                                        .focus_visible(|style| {
                                            style.border_1().border_color(rgb(BLUE))
                                        })
                                        .w(px(24.0))
                                        .h(px(24.0))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded_md()
                                        .text_color(rgb(MUTED))
                                        .child("▯")
                                        .hover(|style| style.bg(rgb(ELEVATED)).cursor_pointer())
                                        .on_click(cx.listener(|view, _, _, cx| {
                                            view.toggle_sidebar(cx);
                                        })),
                                ),
                        )
                    })
                    .when(self.selected_session.is_none(), |main| {
                        main.child(self.home(compact, cx))
                    })
                    .when(self.selected_session.is_some(), |main| {
                        main.child(
                            div()
                                .h(px(56.0))
                                .flex()
                                .items_center()
                                .justify_between()
                                .px_5()
                                .border_b_1()
                                .border_color(rgb(BORDER))
                                .child(div().flex().flex_col().child(div().child(title)).child(
                                    div().text_xs().text_color(rgb(MUTED)).child(format!(
                                        "{} · {}",
                                        workspace.display(),
                                        branch
                                    )),
                                ))
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_3()
                                        .child(
                                            div()
                                                .id("panel-toggle")
                                                .accessibility_id("panel-toggle")
                                                .role(Role::Button)
                                                .aria_label("Toggle session inspector")
                                                .aria_expanded(self.panel_open)
                                                .focusable()
                                                .tab_stop(true)
                                                .focus_visible(|style| {
                                                    style.border_1().border_color(rgb(BLUE))
                                                })
                                                .px_2()
                                                .py_1()
                                                .rounded_md()
                                                .text_xs()
                                                .text_color(rgb(MUTED))
                                                .child(changes_badge(self.selected()))
                                                .hover(|style| {
                                                    style.bg(rgb(ELEVATED)).cursor_pointer()
                                                })
                                                .on_click(cx.listener(|view, _, _, cx| {
                                                    view.panel_open = !view.panel_open;
                                                    cx.notify();
                                                })),
                                        )
                                        .child(
                                            div()
                                                .id("provider-status")
                                                .role(Role::Status)
                                                .aria_label(provider_status.clone())
                                                .text_xs()
                                                .text_color(rgb(provider_color))
                                                .child(provider_status),
                                        ),
                                ),
                        )
                    })
                    .when(self.selected_session.is_some(), |main| {
                        main.child(
                            div()
                                .flex()
                                .flex_1()
                                .min_h_0()
                                .child(self.transcript())
                                .when_some(
                                    if self.panel_open {
                                        self.side_panel(cx)
                                    } else {
                                        None
                                    },
                                    gpui::ParentElement::child,
                                ),
                        )
                    })
                    .when_some(self.action_error.clone(), |column, error| {
                        column.child(
                            div()
                                .id("action-error")
                                .role(Role::Alert)
                                .aria_label(error.clone())
                                .mx_auto()
                                .mb_2()
                                .text_sm()
                                .text_color(rgb(RED))
                                .child(error),
                        )
                    })
                    .when(self.selected_session.is_some(), |main| {
                        main.child(div().w_full().px_5().child(self.session_composer(cx)))
                    }),
            )
            .when(self.open_control_menu.is_some(), |root| {
                root.child(
                    div()
                        .id("dismiss-control-menu")
                        .absolute()
                        .inset_0()
                        .on_mouse_up(
                            MouseButton::Left,
                            cx.listener(|view, _, _, cx| view.dismiss_control_menu(cx)),
                        ),
                )
            })
            .when_some(self.control_menu(cx), |root, menu| {
                root.child(
                    div()
                        .absolute()
                        .left(px(content_left))
                        .right_0()
                        .when(session_selected, |popup| popup.bottom(px(104.0)))
                        .when(!session_selected, |popup| {
                            popup.top(if compact { px(310.0) } else { px(332.0) })
                        })
                        .flex()
                        .justify_center()
                        .child(
                            div()
                                .w_full()
                                .max_w(px(820.0))
                                .pl(px(f32::from(control_menu_left)))
                                .child(menu),
                        ),
                )
            })
            .when(self.session_menu.is_some(), |root| {
                root.child(
                    div()
                        .id("dismiss-session-menu")
                        .absolute()
                        .inset_0()
                        // Dismiss on mouse up, not mouse down: tearing the menu
                        // down on press removes the item before its click can
                        // complete on release.
                        //
                        // Only the left button dismisses. The right-click that
                        // opens the menu releases *after* this overlay exists,
                        // so a right-button handler here would immediately
                        // close the menu the same click just opened.
                        .on_mouse_up(
                            MouseButton::Left,
                            cx.listener(|view, _, _, cx| view.dismiss_session_menu(cx)),
                        ),
                )
            })
            .when_some(self.session_context_menu(cx), gpui::ParentElement::child)
            .when_some(self.rename_dialog(cx), gpui::ParentElement::child)
            .when_some(self.interaction_dialog(cx), |root, dialog| {
                root.child(dialog)
            })
    }
}

/// Trailing slice of terminal output kept for display.
///
/// Phase 3 renders a bounded tail; Phase 6 replaces this with the virtualized
/// terminal and real scrollback.
fn terminal_tail(output: &str) -> String {
    tail_lines(output, 40)
}

/// The last `max_lines` lines of `output`.
fn tail_lines(output: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = output.lines().collect();
    if lines.len() <= max_lines {
        return output.to_owned();
    }
    lines[lines.len() - max_lines..].join("\n")
}

/// Label for the inspector toggle, summarizing changed files at a glance.
fn changes_badge(session: Option<&SessionProjection>) -> String {
    session.map_or_else(
        || "Inspector".to_owned(),
        |session| {
            let changed = session.snapshot.changes.files.len();
            let terminals = session.snapshot.tool_activity.active_terminals().len();
            let blocking = session.snapshot.capabilities.blocking().len();
            let mut parts = vec![format!("{changed} changed")];
            if terminals > 0 {
                parts.push(format!("{terminals} running"));
            }
            if blocking > 0 {
                parts.push(format!("{blocking} blocked"));
            }
            parts.join(" · ")
        },
    )
}

fn control_pill(
    id: &'static str,
    value: String,
    menu: ControlMenu,
    expanded: bool,
    cx: &mut Context<SessionMvpView>,
) -> impl IntoElement {
    let label = match menu {
        ControlMenu::Project => "Project",
        ControlMenu::Location => "Where to run this session",
        ControlMenu::Mode => "Mode",
        ControlMenu::Model => "Model",
        ControlMenu::Effort => "Reasoning effort",
        ControlMenu::Context => "Context length",
    };
    div()
        .id(id)
        .accessibility_id(id)
        .role(Role::ComboBox)
        .aria_label(label)
        .aria_value(value.clone())
        .aria_expanded(expanded)
        .focusable()
        .tab_stop(true)
        .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
        .px_3()
        .py_1()
        .rounded_md()
        .bg(rgb(ELEVATED))
        .text_xs()
        .text_color(rgb(MUTED))
        .child(value)
        .hover(|style| style.text_color(rgb(PRIMARY)).cursor_pointer())
        .on_click(cx.listener(move |view, _, _, cx| {
            view.toggle_control_menu(menu);
            cx.notify();
        }))
}

fn context_readout(value: String) -> impl IntoElement {
    div()
        .id("context")
        .accessibility_id("context")
        .role(Role::Definition)
        .aria_label("Context length")
        .px_3()
        .py_1()
        .text_xs()
        .text_color(rgb(MUTED))
        .child(value)
}

fn control_menu_id(menu: ControlMenu) -> &'static str {
    match menu {
        ControlMenu::Project => "project",
        ControlMenu::Location => "location",
        ControlMenu::Mode => "mode",
        ControlMenu::Model => "model",
        ControlMenu::Effort => "effort",
        ControlMenu::Context => "context",
    }
}

fn disabled_destination(
    id: &'static str,
    icon: &'static str,
    label: &'static str,
) -> impl IntoElement {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap_3()
        .px_3()
        .py_2()
        .text_color(rgb(MUTED))
        .child(icon)
        .child(label)
        .child(div().flex_1())
        .child(div().text_xs().child("Unavailable"))
}

fn compact_layout(width: f32) -> bool {
    width < COMPACT_WIDTH
}

fn control_menu_offset(menu: ControlMenu) -> u16 {
    match menu {
        // The project and location pills sit in the checkout strip below the
        // composer, left to right.
        ControlMenu::Project => 0,
        ControlMenu::Location => 96,
        ControlMenu::Mode => 40,
        ControlMenu::Model => 128,
        ControlMenu::Effort => 216,
        ControlMenu::Context => 304,
    }
}

fn toggled_menu(current: Option<ControlMenu>, requested: ControlMenu) -> Option<ControlMenu> {
    (current != Some(requested)).then_some(requested)
}

fn title_case(value: &str) -> String {
    let mut characters = value.chars();
    characters.next().map_or_else(String::new, |first| {
        first.to_uppercase().collect::<String>() + characters.as_str()
    })
}

fn effort_label(value: &str) -> String {
    match value {
        "xhigh" => "Extra high".to_owned(),
        other => title_case(other),
    }
}

fn reasoning_effort_for_model(supported_efforts: &[String], selected: &str) -> Option<String> {
    (!supported_efforts.is_empty()).then(|| selected.to_owned())
}

fn default_context_tier(windows: &[ContextWindowOption]) -> Option<String> {
    windows
        .iter()
        .find(|window| window.tier == "default")
        .or_else(|| windows.first())
        .map(|window| window.tier.clone())
}

fn context_window_label(window: &ContextWindowOption) -> String {
    window.max_tokens.map_or_else(
        || match window.tier.as_str() {
            "long_context" => "Long context".to_owned(),
            other => title_case(other),
        },
        |tokens| format!("{} context", token_label(tokens)),
    )
}

fn token_label(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        let tenths = tokens / 100_000;
        if tenths.is_multiple_of(10) {
            format!("{}M", tenths / 10)
        } else {
            format!("{}.{}M", tenths / 10, tenths % 10)
        }
    } else if tokens >= 1_000 {
        format!("{}K", tokens / 1_000)
    } else {
        tokens.to_string()
    }
}

fn action_button(
    label: &'static str,
    color: u32,
    cx: &mut Context<SessionMvpView>,
    action: impl Fn(&mut SessionMvpView) + 'static,
) -> impl IntoElement {
    div()
        .id(label)
        .accessibility_id(label)
        .role(Role::Button)
        .aria_label(label)
        .focusable()
        .tab_stop(true)
        .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
        .px_4()
        .py_2()
        .rounded_md()
        .bg(rgb(color))
        .text_color(rgb(BACKGROUND))
        .child(label)
        .hover(|style| style.opacity(0.85).cursor_pointer())
        .on_click(cx.listener(move |view, _, _, cx| {
            action(view);
            cx.notify();
        }))
}

fn status_color(status: SessionStatus) -> gpui::Rgba {
    match status {
        SessionStatus::Running | SessionStatus::Starting => rgb(GREEN),
        SessionStatus::Waiting => rgb(AMBER),
        SessionStatus::Failed | SessionStatus::Cancelled => rgb(RED),
        SessionStatus::Idle | SessionStatus::Recovering | SessionStatus::Disconnected => rgb(MUTED),
    }
}

fn choice_response(kind: InteractionKind, choice: &str) -> InteractionResponse {
    match (kind, choice) {
        (InteractionKind::Permission, value) if value.starts_with("Allow") => {
            InteractionResponse::Approve
        }
        (InteractionKind::AutoModeSwitch, "Switch once") => InteractionResponse::Approve,
        (InteractionKind::AutoModeSwitch, "Always switch") => InteractionResponse::Submit {
            value: "always".into(),
            freeform: false,
        },
        (InteractionKind::Permission | InteractionKind::AutoModeSwitch, _) => {
            InteractionResponse::Reject { feedback: None }
        }
        _ => InteractionResponse::Submit {
            value: choice.to_owned().into(),
            freeform: false,
        },
    }
}

fn database_path() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("GCABB_DATA_DIR") {
        return prepare_data_directory(&PathBuf::from(path));
    }
    let base = dirs::data_local_dir()
        .ok_or_else(|| "operating system did not provide a local data directory".to_owned())?;
    prepare_data_directory(&base.join("gcabb"))
}

/// Working directory for chats.
///
/// Chats have no repository, but the CLI still needs a valid working
/// directory. A dedicated folder under the app data directory keeps chat tool
/// activity away from any checkout; if it cannot be created, fall back to the
/// launch directory so chats still work.
fn chats_directory(fallback: &Path) -> PathBuf {
    let Some(base) = dirs::data_local_dir() else {
        return fallback.to_owned();
    };
    let path = base.join("gcabb").join("chats");
    if std::fs::create_dir_all(&path).is_err() {
        return fallback.to_owned();
    }
    path
}

fn prepare_data_directory(path: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(path)
        .map_err(|error| format!("failed to create {}: {error}", path.display()))?;
    Ok(path.join("gcabb.db"))
}

/// The branch currently checked out in `root`.
fn git_branch(root: &Path) -> String {
    git_output(root, &["branch", "--show-current"]).unwrap_or_else(|| "detached".to_owned())
}

/// The repository a worktree belongs to.
///
/// A repository has one main checkout plus any number of linked worktrees, and
/// `git worktree list` reports the main checkout first. Sessions run inside
/// worktrees but belong to the repository; grouping by worktree path would
/// otherwise show one project per worktree instead of one project per
/// repository. Falls back to `root` when it is not a git worktree.
fn repository_root(root: &Path) -> PathBuf {
    git_output(root, &["worktree", "list", "--porcelain"])
        .and_then(|output| {
            output
                .lines()
                .find_map(|line| line.strip_prefix("worktree ").map(str::to_owned))
        })
        .map_or_else(|| root.to_owned(), PathBuf::from)
}

/// The repository's default branch, used as the changes-view base.
///
/// Resolution order is the remote's published HEAD, then conventional local
/// names. This is deliberately not the checked-out branch: comparing a session
/// worktree against its own branch would report no changes at all.
fn default_branch(root: &Path) -> Option<String> {
    if let Some(head) = git_output(
        root,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    ) {
        if let Some(branch) = head.split_once('/').map(|(_, branch)| branch.to_owned()) {
            return Some(branch);
        }
        return Some(head);
    }
    for candidate in ["main", "master"] {
        if git_output(
            root,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{candidate}"),
            ],
        )
        .is_some()
        {
            return Some(candidate.to_owned());
        }
    }
    None
}

fn git_output(root: &Path, args: &[&str]) -> Option<String> {
    std::process::Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn timestamp() -> String {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or_else(
        |_| "0".to_owned(),
        |duration| duration.as_millis().to_string(),
    )
}

fn main() {
    if let Err(error) = init_tracing("gcabb=info") {
        eprintln!("failed to initialize structured tracing: {error}");
    }
    let project_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let branch = git_branch(&project_root);
    let service = match database_path() {
        Ok(path) => AppService::start(project_root.clone(), path),
        Err(error) => AppService::failed(error),
    };
    let chats_workspace = chats_directory(&project_root);

    gpui_platform::application().run(move |cx: &mut App| {
        bind_text_input_keys(cx);
        cx.bind_keys([KeyBinding::new("escape", DismissPopup, None)]);
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.bind_keys([
            KeyBinding::new("tab", FocusNext, None),
            KeyBinding::new("shift-tab", FocusPrevious, None),
        ]);
        let bounds = Bounds::centered(None, size(px(1280.0), px(860.0)), cx);
        let service = service;
        let project_root = project_root.clone();
        let branch = branch.clone();
        let chats_workspace = chats_workspace.clone();
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some("GCABB".into()),
                        ..Default::default()
                    }),
                    app_id: Some(APP_ID.to_owned()),
                    window_min_size: Some(size(px(640.0), px(520.0))),
                    ..Default::default()
                },
                move |_, cx| {
                    cx.new(|cx| {
                        SessionMvpView::new(service, project_root, branch, chats_workspace, cx)
                    })
                },
            )
            .expect("failed to open GCABB window");
        window
            .update(cx, |view, window, cx| {
                window.focus(&view.composer.focus_handle(cx), cx);
            })
            .expect("failed to focus composer");
        cx.activate(true);
    });
}

#[cfg(test)]
mod tests {
    use app_model::ContextWindowOption;

    use super::{
        COMPACT_WIDTH, ControlMenu, compact_layout, context_window_label, control_menu_id,
        control_menu_offset, default_branch, default_context_tier, effort_label,
        reasoning_effort_for_model, repository_root, toggled_menu, token_label,
    };
    use app_model::SessionLocation;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("git runs");
        assert!(output.status.success(), "git {args:?} failed");
    }

    /// A repository with one linked worktree.
    fn repo_with_worktree() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let main = dir.path().join("main");
        std::fs::create_dir_all(&main).expect("create main");
        git(&main, &["init", "--initial-branch=main"]);
        git(&main, &["config", "user.email", "t@example.com"]);
        git(&main, &["config", "user.name", "T"]);
        std::fs::write(main.join("a.txt"), "a\n").expect("write");
        git(&main, &["add", "."]);
        git(&main, &["commit", "-m", "base"]);
        let worktree = dir.path().join("wt");
        git(
            &main,
            &[
                "worktree",
                "add",
                worktree.to_str().unwrap(),
                "-b",
                "feature",
            ],
        );
        (dir, main, worktree)
    }

    /// Adding a worktree folder must resolve to its repository, so adding a
    /// worktree and its main checkout cannot create two projects.
    #[test]
    fn adding_a_worktree_folder_resolves_to_the_repository() {
        let (_guard, main, worktree) = repo_with_worktree();
        assert_eq!(repository_root(&worktree), main);
        assert_eq!(repository_root(&main), main);
    }

    /// A plain directory that is not a repository is still usable as a project.
    #[test]
    fn adding_a_non_repository_folder_keeps_the_folder() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(repository_root(dir.path()), dir.path());
        assert!(default_branch(dir.path()).is_none());
    }

    /// The changes base must be the repository default, never the branch a
    /// worktree happens to have checked out.
    #[test]
    fn default_branch_is_the_repository_default_not_the_checked_out_branch() {
        let (_guard, main, worktree) = repo_with_worktree();
        assert_eq!(default_branch(&main).as_deref(), Some("main"));
        assert_eq!(default_branch(&worktree).as_deref(), Some("main"));
    }

    /// New worktree is the default so sessions do not disturb the checkout
    /// the developer is using.
    #[test]
    fn new_worktree_is_the_default_location() {
        assert_eq!(SessionLocation::default(), SessionLocation::NewWorktree);
        assert_eq!(SessionLocation::NewWorktree.label(), "New worktree");
        assert_eq!(SessionLocation::LocalRepository.label(), "Local repository");
        assert_eq!(
            SessionLocation::from_str_or_default("local-repository"),
            SessionLocation::LocalRepository
        );
        // Unknown values fall back to the safe option rather than the shared
        // checkout.
        assert_eq!(
            SessionLocation::from_str_or_default("nonsense"),
            SessionLocation::NewWorktree
        );
    }

    #[test]
    fn branch_slugs_are_git_safe() {
        assert_eq!(super::slugify("Fix the login bug!"), "fix-the-login-bug");
        assert_eq!(super::slugify("   "), "session");
        assert!(super::slugify(&"x".repeat(200)).len() <= 40);
    }

    /// A worktree session gets its own checkout on its own branch.
    #[test]
    fn new_worktree_location_creates_a_separate_checkout() {
        let (_guard, main, _worktree) = repo_with_worktree();
        let roots = tempfile::tempdir().expect("tempdir");
        let title = "Add a feature";
        let resolved = super::resolve_session_workspace(
            SessionLocation::NewWorktree,
            app_model::SessionKind::Project,
            &main,
            Some(&main.to_string_lossy()),
            Some("main"),
            title,
            roots.path(),
        )
        .expect("worktree resolved");

        assert_ne!(resolved, main, "the session must not run in the checkout");
        assert!(resolved.join("a.txt").exists(), "checkout is populated");
        let service = git_service::GitService::new(&resolved);
        assert_eq!(service.current_branch().unwrap(), "gcabb/add-a-feature");
        // The developer's checkout is untouched.
        assert_eq!(
            git_service::GitService::new(&main)
                .current_branch()
                .unwrap(),
            "main"
        );
    }

    /// Local repository runs in place, which is the shared-checkout option.
    #[test]
    fn local_repository_location_runs_in_the_project_directory() {
        let (_guard, main, _worktree) = repo_with_worktree();
        let roots = tempfile::tempdir().expect("tempdir");
        let resolved = super::resolve_session_workspace(
            SessionLocation::LocalRepository,
            app_model::SessionKind::Project,
            &main,
            Some(&main.to_string_lossy()),
            Some("main"),
            "Anything",
            roots.path(),
        )
        .expect("resolved");
        assert_eq!(resolved, main);
    }

    /// Chats have no repository, so they never get a worktree.
    #[test]
    fn chats_never_get_a_worktree() {
        let dir = tempfile::tempdir().expect("tempdir");
        let roots = tempfile::tempdir().expect("tempdir");
        let resolved = super::resolve_session_workspace(
            SessionLocation::NewWorktree,
            app_model::SessionKind::Chat,
            dir.path(),
            None,
            None,
            "A chat",
            roots.path(),
        )
        .expect("resolved");
        assert_eq!(resolved, dir.path());
    }

    /// A folder that is not a repository cannot host a worktree, so it runs
    /// in place rather than failing.
    #[test]
    fn non_repository_projects_run_in_place() {
        let dir = tempfile::tempdir().expect("tempdir");
        let roots = tempfile::tempdir().expect("tempdir");
        let resolved = super::resolve_session_workspace(
            SessionLocation::NewWorktree,
            app_model::SessionKind::Project,
            dir.path(),
            Some(&dir.path().to_string_lossy()),
            None,
            "Anything",
            roots.path(),
        )
        .expect("resolved");
        assert_eq!(resolved, dir.path());
    }

    /// Two sessions with the same title must not collide on a branch name.
    #[test]
    fn repeated_titles_get_distinct_branches() {
        let (_guard, main, _worktree) = repo_with_worktree();
        let roots = tempfile::tempdir().expect("tempdir");
        let first = super::resolve_session_workspace(
            SessionLocation::NewWorktree,
            app_model::SessionKind::Project,
            &main,
            Some(&main.to_string_lossy()),
            Some("main"),
            "Same title",
            roots.path(),
        )
        .expect("first worktree");
        let second = super::resolve_session_workspace(
            SessionLocation::NewWorktree,
            app_model::SessionKind::Project,
            &main,
            Some(&main.to_string_lossy()),
            Some("main"),
            "Same title",
            roots.path(),
        )
        .expect("second worktree");

        assert_ne!(first, second);
        assert_eq!(
            git_service::GitService::new(&second)
                .current_branch()
                .unwrap(),
            "gcabb/same-title-2"
        );
    }

    fn window(tier: &str, max_tokens: Option<u64>) -> ContextWindowOption {
        ContextWindowOption {
            tier: tier.to_owned(),
            max_tokens,
        }
    }

    #[test]
    fn compact_layout_uses_stable_breakpoint() {
        assert!(compact_layout(COMPACT_WIDTH - 1.0));
        assert!(!compact_layout(COMPACT_WIDTH));
    }

    #[test]
    fn selector_menu_opens_switches_and_closes() {
        assert_eq!(
            toggled_menu(None, ControlMenu::Model),
            Some(ControlMenu::Model)
        );
        assert_eq!(
            toggled_menu(Some(ControlMenu::Model), ControlMenu::Effort),
            Some(ControlMenu::Effort)
        );
        assert_eq!(
            toggled_menu(Some(ControlMenu::Model), ControlMenu::Model),
            None
        );
    }

    #[test]
    fn selector_menus_align_with_their_composer_pills() {
        assert_eq!(control_menu_offset(ControlMenu::Mode), 40);
        assert_eq!(control_menu_offset(ControlMenu::Model), 128);
        assert_eq!(control_menu_offset(ControlMenu::Effort), 216);
    }

    #[test]
    fn selector_accessibility_ids_match_their_triggers() {
        assert_eq!(control_menu_id(ControlMenu::Mode), "mode");
        assert_eq!(control_menu_id(ControlMenu::Model), "model");
        assert_eq!(control_menu_id(ControlMenu::Effort), "effort");
    }

    #[test]
    fn effort_labels_match_menu_copy() {
        assert_eq!(effort_label("medium"), "Medium");
        assert_eq!(effort_label("xhigh"), "Extra high");
    }

    #[test]
    fn context_length_labels_are_human_readable() {
        assert_eq!(token_label(200_000), "200K");
        assert_eq!(token_label(1_000_000), "1M");
        assert_eq!(token_label(1_500_000), "1.5M");
        assert_eq!(
            context_window_label(&window("long_context", Some(1_000_000))),
            "1M context"
        );
        assert_eq!(
            context_window_label(&window("long_context", None)),
            "Long context"
        );
    }

    #[test]
    fn context_tier_defaults_to_the_standard_window() {
        assert_eq!(default_context_tier(&[]), None);
        assert_eq!(
            default_context_tier(&[
                window("long_context", Some(1_000_000)),
                window("default", Some(200_000)),
            ]),
            Some("default".to_owned())
        );
    }

    #[test]
    fn context_selector_only_appears_for_multiple_windows() {
        assert_eq!(control_menu_id(ControlMenu::Context), "context");
        assert_eq!(control_menu_offset(ControlMenu::Context), 304);
    }

    #[test]
    fn reasoning_effort_is_only_submitted_for_supported_models() {
        assert_eq!(reasoning_effort_for_model(&[], "medium"), None);
        assert_eq!(
            reasoning_effort_for_model(&["low".to_owned(), "medium".to_owned()], "medium"),
            Some("medium".to_owned())
        );
    }

    /// View-level interaction tests.
    ///
    /// These drive the real GPUI element tree with simulated mouse input,
    /// which is the only way to catch event-wiring mistakes such as a dismiss
    /// overlay consuming the click meant for a menu item.
    mod interaction {
        use app_model::{SessionKind, SessionMetadata, SessionSnapshot};
        use gpui::{Modifiers, MouseButton, TestAppContext, VisualTestContext};
        use session_manager::SessionHandle;

        use crate::{AppService, ServiceCommand, SessionMvpView, SessionProjection};

        fn snapshot(id: &str, title: &str) -> SessionSnapshot {
            let mut state = SessionSnapshot::new(SessionMetadata {
                id: id.to_owned(),
                sdk_session_id: format!("sdk-{id}"),
                project_path: "/tmp/project".to_owned(),
                repository_root: Some("/tmp/project".to_owned()),
                title: title.to_owned(),
                kind: SessionKind::Project,
                model: None,
                mode: None,
                base_ref: None,
                created_at: "1".to_owned(),
                updated_at: "1".to_owned(),
            });
            state.status = app_model::SessionStatus::Idle;
            state
        }

        /// Build the real view with one session row rendered.
        fn setup(
            cx: &mut TestAppContext,
        ) -> (
            gpui::Entity<SessionMvpView>,
            &mut VisualTestContext,
            std::sync::mpsc::Receiver<ServiceCommand>,
        ) {
            let (service, commands) = AppService::for_test();
            cx.update(ui_components::bind_text_input_keys);
            let (view, cx) = cx.add_window_view(|_, cx| {
                let mut view = SessionMvpView::new(
                    service,
                    std::path::PathBuf::from("/tmp/project"),
                    "main".to_owned(),
                    std::path::PathBuf::from("/tmp/chats"),
                    cx,
                );
                view.selected_project = std::path::PathBuf::from("/tmp/project");
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(
                    snapshot("session-1", "First session"),
                ))];
                view
            });
            cx.run_until_parked();
            (view, cx, commands)
        }

        /// Regression: the right-click that opens the menu releases after the
        /// dismiss overlay exists. A right-button handler on that overlay made
        /// the menu flash open and vanish on the same click.
        #[gpui::test]
        fn right_click_menu_survives_the_release_of_the_opening_click(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            let row = cx
                .debug_bounds("session-row")
                .expect("session row rendered");

            cx.simulate_mouse_down(row.center(), MouseButton::Right, Modifiers::none());
            cx.run_until_parked();
            view.read_with(cx, |view, _| {
                assert!(view.session_menu.is_some(), "menu should open on press");
            });

            cx.simulate_mouse_up(row.center(), MouseButton::Right, Modifiers::none());
            cx.run_until_parked();
            view.read_with(cx, |view, _| {
                assert!(
                    view.session_menu.is_some(),
                    "menu must survive the release of the click that opened it"
                );
            });
        }

        /// Regression: dismissing on mouse *down* removed the menu item before
        /// its click could complete on release, so Rename never ran.
        #[gpui::test]
        fn clicking_rename_opens_the_rename_dialog(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            let row = cx
                .debug_bounds("session-row")
                .expect("session row rendered");
            cx.simulate_mouse_down(row.center(), MouseButton::Right, Modifiers::none());
            cx.simulate_mouse_up(row.center(), MouseButton::Right, Modifiers::none());
            cx.run_until_parked();

            let item = cx
                .debug_bounds("session-menu-rename")
                .expect("rename item rendered");
            cx.simulate_click(item.center(), Modifiers::none());
            cx.run_until_parked();

            view.read_with(cx, |view, _| {
                assert_eq!(view.renaming_session.as_deref(), Some("session-1"));
                assert!(view.session_menu.is_none(), "menu closes after choosing");
            });
        }

        #[gpui::test]
        fn clicking_delete_sends_a_delete_command(cx: &mut TestAppContext) {
            let (view, cx, commands) = setup(cx);
            let row = cx
                .debug_bounds("session-row")
                .expect("session row rendered");
            cx.simulate_mouse_down(row.center(), MouseButton::Right, Modifiers::none());
            cx.simulate_mouse_up(row.center(), MouseButton::Right, Modifiers::none());
            cx.run_until_parked();

            let item = cx
                .debug_bounds("session-menu-delete")
                .expect("delete item rendered");
            cx.simulate_click(item.center(), Modifiers::none());
            cx.run_until_parked();

            view.read_with(cx, |view, _| assert!(view.session_menu.is_none()));
            let command = commands.try_recv().expect("a command was sent");
            match command {
                ServiceCommand::DeleteSession { app_session_id } => {
                    assert_eq!(app_session_id, "session-1");
                }
                _ => panic!("expected a DeleteSession command"),
            }
        }

        /// Left-clicking away from an open menu still dismisses it.
        #[gpui::test]
        fn clicking_away_dismisses_the_menu(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            let row = cx
                .debug_bounds("session-row")
                .expect("session row rendered");
            cx.simulate_mouse_down(row.center(), MouseButton::Right, Modifiers::none());
            cx.simulate_mouse_up(row.center(), MouseButton::Right, Modifiers::none());
            cx.run_until_parked();
            view.read_with(cx, |view, _| assert!(view.session_menu.is_some()));

            let away = gpui::Point::new(gpui::px(900.0), gpui::px(600.0));
            cx.simulate_click(away, Modifiers::none());
            cx.run_until_parked();
            view.read_with(cx, |view, _| assert!(view.session_menu.is_none()));
        }

        /// Renaming updates the sidebar immediately and asks the service to
        /// persist the new title.
        #[gpui::test]
        fn committing_a_rename_updates_the_row_and_sends_the_command(cx: &mut TestAppContext) {
            let (view, cx, commands) = setup(cx);
            view.update(cx, |view, cx| {
                view.renaming_session = Some("session-1".to_owned());
                view.commit_rename("Renamed", cx);
            });

            view.read_with(cx, |view, _| {
                assert_eq!(view.sessions[0].snapshot.metadata.title, "Renamed");
                assert!(view.renaming_session.is_none());
            });
            match commands.try_recv().expect("a command was sent") {
                ServiceCommand::RenameSession { title, .. } => assert_eq!(title, "Renamed"),
                _ => panic!("expected a RenameSession command"),
            }
        }

        /// The project picker offers Chat first, then projects, then the
        /// folder picker. Chat needs no configuration so it leads.
        #[gpui::test]
        fn project_menu_offers_chat_projects_and_add_project(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                cx.notify();
                view.projects = vec![app_model::ProjectMetadata {
                    id: "/tmp/project".to_owned(),
                    path: "/tmp/project".to_owned(),
                    name: "project".to_owned(),
                    default_branch: Some("main".to_owned()),
                    last_opened_at: "1".to_owned(),
                }];
            });

            let options = view.read_with(cx, |view, _| view.project_options());
            let values: Vec<&str> = options.iter().map(|(value, _, _)| value.as_str()).collect();
            assert_eq!(values.first().copied(), Some(super::super::CHAT_OPTION));
            assert!(values.contains(&"/tmp/project"));
            assert_eq!(
                values.last().copied(),
                Some(super::super::ADD_PROJECT_OPTION)
            );
        }

        /// Choosing Chat switches the composer to a repository-less session.
        #[gpui::test]
        fn choosing_chat_starts_a_chat_session(cx: &mut TestAppContext) {
            let (view, cx, commands) = setup(cx);
            view.update(cx, |view, cx| {
                view.choose_control(
                    super::super::ControlMenu::Project,
                    super::super::CHAT_OPTION.to_owned(),
                    cx,
                );
            });
            view.read_with(cx, |view, _| {
                assert!(view.composing_chat);
                assert!(view.selected_session.is_none());
            });

            view.update(cx, |view, _| view.submit_prompt("hello".to_owned()));
            // The Select command from new_chat comes first.
            let mut submit = None;
            while let Ok(command) = commands.try_recv() {
                if let ServiceCommand::Submit {
                    kind,
                    project_path,
                    repository_root,
                    base_ref,
                    ..
                } = command
                {
                    submit = Some((kind, project_path, repository_root, base_ref));
                }
            }
            let (kind, project_path, repository_root, base_ref) =
                submit.expect("a submit command was sent");
            assert_eq!(kind, SessionKind::Chat);
            assert_eq!(project_path, std::path::PathBuf::from("/tmp/chats"));
            assert!(repository_root.is_none(), "a chat has no repository");
            assert!(base_ref.is_none(), "a chat has no changes base");
        }

        /// Choosing a project returns the composer to project mode.
        #[gpui::test]
        fn choosing_a_project_leaves_chat_mode(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                view.choose_control(
                    super::super::ControlMenu::Project,
                    super::super::CHAT_OPTION.to_owned(),
                    cx,
                );
                view.choose_control(
                    super::super::ControlMenu::Project,
                    "/tmp/project".to_owned(),
                    cx,
                );
            });
            view.read_with(cx, |view, _| assert!(!view.composing_chat));
        }

        /// The hover-revealed plus on the Chats row starts a chat.
        #[gpui::test]
        fn clicking_new_chat_starts_a_chat(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            let plus = cx
                .debug_bounds("new-chat")
                .expect("new chat button rendered");
            cx.simulate_click(plus.center(), Modifiers::none());
            cx.run_until_parked();
            view.read_with(cx, |view, _| assert!(view.composing_chat));
        }

        /// Regression: choosing Chat updated internal state but the composer
        /// still showed the project, so selecting Chat looked like a no-op.
        #[gpui::test]
        fn choosing_chat_updates_the_composer_pill_label(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                view.projects = vec![app_model::ProjectMetadata {
                    id: "/tmp/project".to_owned(),
                    path: "/tmp/project".to_owned(),
                    name: "project".to_owned(),
                    default_branch: Some("main".to_owned()),
                    last_opened_at: "1".to_owned(),
                }];
                cx.notify();
            });
            view.read_with(cx, |view, _| {
                assert_eq!(view.composer_project_label(), "project");
            });

            view.update(cx, |view, cx| {
                view.choose_control(
                    super::super::ControlMenu::Project,
                    super::super::CHAT_OPTION.to_owned(),
                    cx,
                );
            });
            view.read_with(cx, |view, _| {
                assert_eq!(
                    view.composer_project_label(),
                    "Chat",
                    "the pill must show Chat once chat mode is chosen"
                );
                assert!(view.targets_chat());
            });
        }

        /// Regression: the hover plus set state but nothing on screen changed.
        #[gpui::test]
        fn clicking_new_chat_updates_the_composer_pill_label(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            let plus = cx
                .debug_bounds("new-chat")
                .expect("new chat button rendered");
            cx.simulate_click(plus.center(), Modifiers::none());
            cx.run_until_parked();
            view.read_with(cx, |view, _| {
                assert_eq!(view.composer_project_label(), "Chat");
            });
        }

        /// Regression: adding a project while in chat mode selected the new
        /// project in the menu but left the pill showing Chat, because only
        /// the menu's project branch cleared the flag.
        #[gpui::test]
        fn adding_a_project_while_in_chat_mode_leaves_chat_mode(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                view.choose_control(
                    super::super::ControlMenu::Project,
                    super::super::CHAT_OPTION.to_owned(),
                    cx,
                );
            });
            view.read_with(cx, |view, _| {
                assert_eq!(view.composer_project_label(), "Chat");
            });

            // The service reports the newly added project and asks for it to
            // be selected, which is the path add-project takes.
            view.update(cx, |view, cx| {
                view.projects = vec![app_model::ProjectMetadata {
                    id: "/tmp/added".to_owned(),
                    path: "/tmp/added".to_owned(),
                    name: "added".to_owned(),
                    default_branch: Some("main".to_owned()),
                    last_opened_at: "1".to_owned(),
                }];
                view.select_project("/tmp/added", cx);
            });

            view.read_with(cx, |view, _| {
                assert!(
                    !view.targets_chat(),
                    "adding a project must leave chat mode"
                );
                assert_eq!(
                    view.composer_project_label(),
                    "added",
                    "the pill must follow the newly selected project"
                );
            });
        }

        /// The menu's checkmark must agree with the pill.
        #[gpui::test]
        fn project_menu_marks_chat_as_selected_in_chat_mode(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                view.choose_control(
                    super::super::ControlMenu::Project,
                    super::super::CHAT_OPTION.to_owned(),
                    cx,
                );
                view.toggle_control_menu(super::super::ControlMenu::Project);
            });
            view.read_with(cx, |view, _| {
                assert!(view.targets_chat());
                assert_eq!(view.composer_project_label(), "Chat");
            });
        }

        /// Selecting a chat session shows chat context, not a stale branch.
        #[gpui::test]
        fn selecting_a_chat_session_reports_no_repository(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                let mut chat = snapshot("chat-1", "A chat");
                chat.metadata.kind = SessionKind::Chat;
                chat.metadata.repository_root = None;
                view.sessions
                    .push(SessionProjection::for_test(SessionHandle::for_test(chat)));
                view.selected_session = Some("chat-1".to_owned());
                cx.notify();
            });
            view.read_with(cx, |view, _| {
                assert!(view.targets_chat());
                assert_eq!(view.composer_project_label(), "Chat");
            });
        }

        /// Phase 3b: tool calls must be visible in the transcript, not just
        /// the prose around them.
        #[gpui::test]
        fn tool_calls_render_in_the_transcript(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                let mut state = snapshot("session-1", "First session");
                for (index, raw) in [
                    serde_json::json!({"id":"u","type":"user.message",
                        "data":{"content":"fix it"}}),
                    serde_json::json!({"id":"t","type":"tool.execution_start",
                        "data":{"toolCallId":"c1","toolName":"str_replace_editor",
                                "arguments":{"path":"src/lib.rs"}}}),
                    serde_json::json!({"id":"tc","type":"tool.execution_complete",
                        "data":{"toolCallId":"c1","success":true,
                                "result":{"detailedContent":"@@ -1 +1 @@\n-old\n+new"}}}),
                ]
                .into_iter()
                .enumerate()
                {
                    let event = app_model::DomainEvent::from_sdk_event_for(
                        "session-1",
                        u64::try_from(index).unwrap_or(0) + 1,
                        &raw,
                    );
                    state.apply(event);
                }
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(state))];
                view.selected_session = Some("session-1".to_owned());
                cx.notify();
            });
            cx.run_until_parked();

            assert!(
                cx.debug_bounds("tool-entry").is_some(),
                "the edit should be visible in the transcript"
            );
            view.read_with(cx, |view, _| {
                let snapshot = &view.selected().unwrap().snapshot;
                let timeline = snapshot.timeline();
                assert_eq!(timeline.len(), 2, "one message and one tool call");
            });
        }

        /// The command block is capped at a third of the entry budget, so a
        /// long script cannot crowd out the output worth reading.
        #[test]
        fn command_block_is_capped_at_a_third_of_the_entry() {
            assert!(
                (super::super::COMMAND_BLOCK_HEIGHT - super::super::ENTRY_DETAIL_BUDGET / 3.0)
                    .abs()
                    < f32::EPSILON
            );
            let output_height =
                super::super::ENTRY_DETAIL_BUDGET - super::super::COMMAND_BLOCK_HEIGHT;
            assert!(
                output_height > super::super::COMMAND_BLOCK_HEIGHT * 1.9,
                "output should get the majority of the budget"
            );
        }

        /// Regression: scrolling a tool's output also scrolled the transcript
        /// behind it, dragging the whole conversation along.
        #[gpui::test]
        fn scrolling_output_does_not_scroll_the_transcript(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                let mut state = snapshot("session-1", "First session");
                let mut sequence = 0;
                let mut apply =
                    |raw: &serde_json::Value, state: &mut app_model::SessionSnapshot| {
                        sequence += 1;
                        state.apply(app_model::DomainEvent::from_sdk_event_for(
                            "session-1",
                            sequence,
                            raw,
                        ));
                    };
                // Enough messages that the transcript itself can scroll.
                for index in 0..60 {
                    apply(
                        &serde_json::json!({"id": format!("u{index}"), "type":"user.message",
                            "data":{"content": format!("message {index}")}}),
                        &mut state,
                    );
                }
                apply(
                    &serde_json::json!({"id":"t","type":"tool.execution_start",
                        "data":{"toolCallId":"c1","toolName":"bash",
                                "arguments":{"command":"seq 1 500"},
                                "shellToolInfo":{"displayCommand":"seq 1 500",
                                                 "hasWriteFileRedirection":false,
                                                 "possiblePaths":[]}}}),
                    &mut state,
                );
                let output = (1..=500)
                    .map(|n| n.to_string())
                    .collect::<Vec<_>>()
                    .join("\n");
                apply(
                    &serde_json::json!({"id":"p","type":"tool.execution_partial_result",
                        "data":{"toolCallId":"c1","partialOutput": output}}),
                    &mut state,
                );
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(state))];
                view.selected_session = Some("session-1".to_owned());
                cx.notify();
            });
            cx.run_until_parked();

            let wheel = |position, cx: &mut VisualTestContext| {
                cx.simulate_event(gpui::ScrollWheelEvent {
                    position,
                    delta: gpui::ScrollDelta::Pixels(gpui::point(gpui::px(0.0), gpui::px(120.0))),
                    modifiers: Modifiers::none(),
                    touch_phase: gpui::TouchPhase::Moved,
                });
                cx.run_until_parked();
            };

            // Control: over the transcript itself, the wheel must scroll it.
            // Without this the assertion below could pass on a transcript that
            // never scrolls at all.
            let transcript = cx.debug_bounds("transcript").expect("transcript rendered");
            let before = view.read_with(cx, |view, _| view.transcript_scroll.offset().y);
            wheel(
                gpui::point(transcript.center().x, transcript.origin.y + gpui::px(8.0)),
                cx,
            );
            let after_transcript = view.read_with(cx, |view, _| view.transcript_scroll.offset().y);
            assert_ne!(
                before, after_transcript,
                "the control case must scroll the transcript"
            );

            // Over a tool entry's output, only the block moves.
            let block = cx
                .debug_bounds("tool-detail")
                .expect("output block rendered");
            wheel(block.center(), cx);
            let after_block = view.read_with(cx, |view, _| view.transcript_scroll.offset().y);
            assert_eq!(
                after_transcript, after_block,
                "the transcript must not move when scrolling inside a tool entry"
            );
        }

        /// Regression: command output was clipped inside a tool entry, so the
        /// end of a long run was unreachable.
        #[gpui::test]
        fn tool_output_scrolls_inside_the_entry(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                let mut state = snapshot("session-1", "First session");
                let mut sequence = 0;
                let mut apply =
                    |raw: &serde_json::Value, state: &mut app_model::SessionSnapshot| {
                        sequence += 1;
                        state.apply(app_model::DomainEvent::from_sdk_event_for(
                            "session-1",
                            sequence,
                            raw,
                        ));
                    };
                apply(
                    &serde_json::json!({"id":"t","type":"tool.execution_start",
                        "data":{"toolCallId":"c1","toolName":"bash",
                                "arguments":{"command":"seq 1 500"},
                                "shellToolInfo":{"displayCommand":"seq 1 500",
                                                 "hasWriteFileRedirection":false,
                                                 "possiblePaths":[]}}}),
                    &mut state,
                );
                let output = (1..=500)
                    .map(|n| n.to_string())
                    .collect::<Vec<_>>()
                    .join("\n");
                apply(
                    &serde_json::json!({"id":"p","type":"tool.execution_partial_result",
                        "data":{"toolCallId":"c1","partialOutput": output}}),
                    &mut state,
                );
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(state))];
                view.selected_session = Some("session-1".to_owned());
                cx.notify();
            });
            cx.run_until_parked();

            let bounds = cx
                .debug_bounds("tool-detail")
                .expect("output block rendered");
            // The block is bounded, so 500 lines of output cannot stretch the
            // entry to the height of the conversation.
            let budget = super::super::ENTRY_DETAIL_BUDGET - super::super::COMMAND_BLOCK_HEIGHT;
            assert!(
                bounds.size.height <= gpui::px(budget),
                "the entry stays compact, got {:?}",
                bounds.size.height
            );
        }

        /// Regression: the transcript clipped its overflow, so a long
        /// conversation ran off the bottom of the window with no way to reach
        /// it. It must scroll, and follow new output as it arrives.
        #[gpui::test]
        fn transcript_scrolls_and_follows_new_output(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                let mut state = snapshot("session-1", "First session");
                for index in 0..80 {
                    state.transcript.push(app_model::TranscriptMessage {
                        id: format!("m{index}"),
                        role: app_model::TranscriptRole::Assistant,
                        content: format!("message {index} with enough text to take a line"),
                        state: app_model::TranscriptState::Complete,
                        timestamp: "1".to_owned(),
                        sequence: u64::try_from(index).unwrap_or(0) + 1,
                    });
                }
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(state))];
                view.selected_session = Some("session-1".to_owned());
                cx.notify();
            });
            cx.run_until_parked();

            // Auto-follow leaves the view at the tail, so scroll back up.
            let before = view.read_with(cx, |view, _| view.transcript_scroll.offset().y);
            assert!(
                before < gpui::px(0.0),
                "the transcript should be scrolled to the newest output, got {before:?}"
            );
            // A wheel event over the transcript must move it. A clipped
            // container swallows the event and the offset stays put.
            let bounds = cx.debug_bounds("transcript").expect("transcript rendered");
            cx.simulate_event(gpui::ScrollWheelEvent {
                position: bounds.center(),
                delta: gpui::ScrollDelta::Pixels(gpui::point(gpui::px(0.0), gpui::px(400.0))),
                modifiers: Modifiers::none(),
                touch_phase: gpui::TouchPhase::Moved,
            });
            cx.run_until_parked();
            let after = view.read_with(cx, |view, _| view.transcript_scroll.offset().y);
            assert!(
                after > before,
                "scrolling up must move the transcript: {before:?} -> {after:?}"
            );

            // Every message is rendered, so scrolling up reaches the start.
            view.read_with(cx, |view, _| {
                assert_eq!(view.selected().unwrap().snapshot.transcript.len(), 80);
            });
        }

        /// Switching sessions and receiving new output both scroll to the tail,
        /// but an unchanged transcript does not, so reading is not interrupted.
        #[gpui::test]
        fn transcript_only_follows_when_it_grows(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                let mut state = snapshot("session-1", "First session");
                state.transcript.push(app_model::TranscriptMessage {
                    id: "m0".to_owned(),
                    role: app_model::TranscriptRole::Assistant,
                    content: "first".to_owned(),
                    state: app_model::TranscriptState::Complete,
                    timestamp: "1".to_owned(),
                    sequence: 1,
                });
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(state))];
                view.selected_session = Some("session-1".to_owned());
                cx.notify();
            });
            cx.run_until_parked();

            // The first render adopts the session and records its extent.
            let extent = view.read_with(cx, |view, _| view.transcript_extent.clone());
            assert_eq!(extent.0, "session-1");
            assert_eq!(extent.1, 1);

            // Re-rendering without new output leaves the recorded extent alone.
            view.update(cx, |_, cx| cx.notify());
            cx.run_until_parked();
            let unchanged = view.read_with(cx, |view, _| view.transcript_extent.clone());
            assert_eq!(unchanged, extent);
        }

        /// Regression: selecting a chat repointed the project selection at the
        /// chats directory, which hid every project session in the sidebar.
        #[gpui::test]
        fn selecting_a_chat_keeps_project_sessions_visible(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                let mut chat = snapshot("chat-1", "A chat");
                chat.metadata.kind = SessionKind::Chat;
                chat.metadata.repository_root = None;
                chat.metadata.project_path = "/tmp/chats".to_owned();
                view.sessions
                    .push(SessionProjection::for_test(SessionHandle::for_test(chat)));
                cx.notify();
            });
            cx.run_until_parked();
            assert!(
                cx.debug_bounds("session-row").is_some(),
                "the project session should be listed before selecting a chat"
            );

            view.update(cx, |view, cx| {
                view.select_session("chat-1".to_owned(), cx);
            });
            cx.run_until_parked();

            view.read_with(cx, |view, _| {
                assert_eq!(
                    view.selected_project,
                    std::path::PathBuf::from("/tmp/project"),
                    "a chat must not repoint the project selection"
                );
            });
            assert!(
                cx.debug_bounds("session-row").is_some(),
                "project sessions must stay visible while a chat is selected"
            );
        }

        /// Regression: removing the last project left the pill naming the
        /// launch directory, which was not a configured project.
        #[gpui::test]
        fn removing_the_last_project_falls_back_to_chat(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                view.projects = vec![app_model::ProjectMetadata {
                    id: "/tmp/project".to_owned(),
                    path: "/tmp/project".to_owned(),
                    name: "project".to_owned(),
                    default_branch: Some("main".to_owned()),
                    last_opened_at: "1".to_owned(),
                }];
                cx.notify();
            });
            view.read_with(cx, |view, _| {
                assert_eq!(view.composer_project_label(), "project");
            });

            // The service reports an empty project list after a removal.
            view.update(cx, |view, cx| {
                view.projects = Vec::new();
                view.composing_chat = true;
                view.selected_session = None;
                cx.notify();
            });

            view.read_with(cx, |view, _| {
                assert_eq!(
                    view.composer_project_label(),
                    "Chat",
                    "with no projects configured the composer targets chat"
                );
            });
        }

        /// An unconfigured project selection must not be named as if it were
        /// a project.
        #[gpui::test]
        fn unknown_project_selection_reads_as_no_project(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                view.projects = Vec::new();
                view.composing_chat = false;
                view.selected_project = std::path::PathBuf::from("/tmp/not-a-project");
                cx.notify();
            });
            view.read_with(cx, |view, _| {
                assert_eq!(view.composer_project_label(), "No project");
            });
        }

        fn model(id: &str, efforts: &[&str], context: Option<u64>) -> app_model::ModelOption {
            app_model::ModelOption {
                id: id.to_owned(),
                name: "GPT-5.6 Sol".to_owned(),
                supported_reasoning_efforts: efforts
                    .iter()
                    .map(|effort| (*effort).to_owned())
                    .collect(),
                context_windows: context
                    .map(|max_tokens| app_model::ContextWindowOption {
                        tier: "default".to_owned(),
                        max_tokens: Some(max_tokens),
                    })
                    .into_iter()
                    .collect(),
            }
        }

        /// A model that exposes an extended context tier.
        fn two_tier_model(id: &str, efforts: &[&str]) -> app_model::ModelOption {
            app_model::ModelOption {
                id: id.to_owned(),
                name: "GPT-5.6 Sol".to_owned(),
                supported_reasoning_efforts: efforts
                    .iter()
                    .map(|effort| (*effort).to_owned())
                    .collect(),
                context_windows: vec![
                    app_model::ContextWindowOption {
                        tier: "default".to_owned(),
                        max_tokens: Some(400_000),
                    },
                    app_model::ContextWindowOption {
                        tier: "long_context".to_owned(),
                        max_tokens: Some(1_050_000),
                    },
                ],
            }
        }

        /// Regression: the per-session model catalog can list a model without
        /// its reasoning efforts, which made the thinking-level pill vanish as
        /// soon as a session was selected.
        #[gpui::test]
        fn session_keeps_the_thinking_level_pill_from_the_app_catalog(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                // The app catalog knows the model's capabilities.
                view.startup =
                    super::super::StartupState::Ready(copilot_provider::ProviderCompatibility {
                        sdk_crate_version: "test".to_owned(),
                        sdk_protocol_version: 3,
                        negotiated_protocol_version: 3,
                        process_id: None,
                        startup: None,
                        available_modes: vec!["interactive".to_owned()],
                        available_models: vec![model(
                            "gpt-5.6-sol",
                            &["low", "medium", "high"],
                            Some(1_000_000),
                        )],
                    });
                view.draft_model = Some("gpt-5.6-sol".to_owned());
                cx.notify();
            });
            // Home has the pill.
            view.read_with(cx, |view, _| {
                assert!(!view.effort_options().is_empty());
                assert_eq!(view.draft_model_label(), "GPT-5.6 Sol");
            });

            // Selecting a session whose catalog omits the capability detail
            // must not lose either the thinking level or the context length.
            view.update(cx, |view, cx| {
                let mut state = snapshot("session-1", "First session");
                state.controls.available_models = vec![model("gpt-5.6-sol", &[], None)];
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(state))];
                view.selected_session = Some("session-1".to_owned());
                cx.notify();
            });

            view.read_with(cx, |view, _| {
                assert!(
                    !view.effort_options().is_empty(),
                    "the session composer must offer the same thinking levels"
                );
                assert_eq!(
                    view.draft_context_label().as_deref(),
                    Some("1M context"),
                    "the session composer must show the same context length"
                );
            });
        }

        /// The app catalog is authoritative for capabilities, because the
        /// per-session catalog collapses context tiers into the active window
        /// and reports no reasoning efforts at all.
        #[gpui::test]
        fn app_catalog_is_authoritative_for_capabilities(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                view.startup =
                    super::super::StartupState::Ready(copilot_provider::ProviderCompatibility {
                        sdk_crate_version: "test".to_owned(),
                        sdk_protocol_version: 3,
                        negotiated_protocol_version: 3,
                        process_id: None,
                        startup: None,
                        available_modes: vec!["interactive".to_owned()],
                        available_models: vec![two_tier_model("gpt-5.6-sol", &["low", "medium"])],
                    });
                let mut state = snapshot("session-1", "First session");
                // Shaped like the live session catalog: no efforts, and the
                // tiers collapsed into a single active window.
                state.controls.available_models = vec![model("gpt-5.6-sol", &[], Some(1_050_000))];
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(state))];
                view.selected_session = Some("session-1".to_owned());
                view.draft_model = Some("gpt-5.6-sol".to_owned());
                cx.notify();
            });
            view.read_with(cx, |view, _| {
                assert_eq!(view.effort_options().len(), 2);
                // Both tiers stay selectable, so the control stays a picker
                // instead of degrading to static text.
                assert_eq!(view.draft_context_windows().len(), 2);
            });
        }

        /// Regression: the branch beside the location pill showed the branch of
        /// the directory GCABB was launched from, not the session's base.
        #[gpui::test]
        fn branch_label_follows_the_project_and_location(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                view.projects = vec![app_model::ProjectMetadata {
                    id: "/tmp/project".to_owned(),
                    path: "/tmp/project".to_owned(),
                    name: "project".to_owned(),
                    default_branch: Some("main".to_owned()),
                    last_opened_at: "1".to_owned(),
                }];
                view.selected_project = std::path::PathBuf::from("/tmp/project");
                // The launch directory is on an unrelated branch.
                view.branch = "launch-worktree-branch".to_owned();
                view.project_branch = Some("feature".to_owned());
                cx.notify();
            });

            // A new worktree branches from the project default.
            view.read_with(cx, |view, _| {
                assert_eq!(view.composer_branch_label(), "main");
            });

            view.update(cx, |view, cx| {
                view.choose_control(
                    super::super::ControlMenu::Location,
                    app_model::SessionLocation::LocalRepository
                        .as_str()
                        .to_owned(),
                    cx,
                );
            });
            // Running in place uses the branch that checkout has now.
            view.read_with(cx, |view, _| {
                assert_eq!(view.composer_branch_label(), "feature");
                assert_ne!(view.composer_branch_label(), view.branch);
            });
        }

        /// The location pill is offered for projects and switches the target.
        #[gpui::test]
        fn location_pill_switches_where_the_session_runs(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.read_with(cx, |view, _| {
                assert_eq!(view.draft_location, app_model::SessionLocation::NewWorktree);
            });
            assert!(
                cx.debug_bounds("location-pill").is_some(),
                "a project session offers a location"
            );

            view.update(cx, |view, cx| {
                view.choose_control(
                    super::super::ControlMenu::Location,
                    app_model::SessionLocation::LocalRepository
                        .as_str()
                        .to_owned(),
                    cx,
                );
            });
            view.read_with(cx, |view, _| {
                assert_eq!(
                    view.draft_location,
                    app_model::SessionLocation::LocalRepository
                );
            });
        }

        /// A chat has no checkout, so it must not offer a location.
        #[gpui::test]
        fn chats_do_not_offer_a_location(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                view.choose_control(
                    super::super::ControlMenu::Project,
                    super::super::CHAT_OPTION.to_owned(),
                    cx,
                );
            });
            cx.run_until_parked();
            assert!(
                cx.debug_bounds("location-pill").is_none(),
                "a chat has no checkout to choose"
            );
        }

        /// Chats are listed under Chats, not under a project.
        #[gpui::test]
        fn chats_are_listed_separately_from_project_sessions(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                let mut chat = snapshot("chat-1", "A chat");
                chat.metadata.kind = SessionKind::Chat;
                chat.metadata.repository_root = None;
                view.sessions
                    .push(SessionProjection::for_test(SessionHandle::for_test(chat)));
                cx.notify();
            });
            cx.run_until_parked();

            assert!(
                cx.debug_bounds("chat-row").is_some(),
                "the chat should render under Chats"
            );
            view.read_with(cx, |view, _| {
                let chats: Vec<_> = view
                    .sessions
                    .iter()
                    .filter(|session| session.snapshot.metadata.is_chat())
                    .collect();
                assert_eq!(chats.len(), 1);
            });
        }

        /// An empty name would leave the row unidentifiable, so it is ignored.
        #[gpui::test]
        fn blank_renames_are_ignored(cx: &mut TestAppContext) {
            let (view, cx, commands) = setup(cx);
            view.update(cx, |view, cx| {
                view.renaming_session = Some("session-1".to_owned());
                view.commit_rename("   ", cx);
            });

            view.read_with(cx, |view, _| {
                assert_eq!(view.sessions[0].snapshot.metadata.title, "First session");
            });
            assert!(commands.try_recv().is_err(), "no command should be sent");
        }
    }
}
