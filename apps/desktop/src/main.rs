use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use app_model::{
    ContextWindowOption, InteractionKind, InteractionResponse, ProjectMetadata, PromptAttachment,
    SessionKind, SessionLocation, SessionMetadata, SessionSnapshot, SessionStatus, TitleSource,
    TranscriptRole, TranscriptState,
};
use copilot_provider::{CopilotProvider, ProviderCompatibility};
use diagnostics::{DiagnosticEvent, DiagnosticsSink, TracingDiagnostics, init_tracing};
use git_service::GitService;
use gpui::prelude::FluentBuilder;
use gpui::{
    App, AppContext, Bounds, Context, Entity, ExternalPaths, FocusHandle, Focusable,
    InteractiveElement, IntoElement, KeyBinding, MouseButton, ParentElement, PathPromptOptions,
    Render, Role, SharedString, StatefulInteractiveElement, Styled, TitlebarOptions, Window,
    WindowBounds, WindowOptions, actions, div, px, rgb, size,
};
use session_manager::{
    CreateSessionRequest, RestoreFailure, SessionHandle, SessionManager, SessionRoots,
    WorktreeOutcome,
};
use storage::Storage;
use tokio::sync::watch;
use ui_components::{ImagesPasted, InputSubmitted, PastedImage, TextInput, bind_text_input_keys};
use updater::install::InstallLayout;
use updater::version::BuildStamp;

mod markdown;
mod updates;

use markdown::{MarkdownNode, MarkdownTag};
use updates::{UpdateRequest, UpdateService, UpdateUi};

const BACKGROUND: u32 = 0x000d_1117;
const SIDEBAR: u32 = 0x0016_1b22;
const PANEL: u32 = 0x000d_1117;
const ELEVATED: u32 = 0x0021_262d;
const SUBTLE: u32 = 0x001b_222c;
const BORDER: u32 = 0x0030_363d;
const PRIMARY: u32 = 0x00f0_f3f6;
const MUTED: u32 = 0x008b_949e;
const GREEN: u32 = 0x003f_b950;
const DATA_DIRECTORY_NAME: &str = "GCABB-data";
const PERSISTENT_DATA_ENTRIES: &[&str] = &[
    "gcabb.db",
    "gcabb.db-shm",
    "gcabb.db-wal",
    "update-settings.json",
    "attachments",
    "chats",
    "worktrees",
];
const BLUE: u32 = 0x0058_a6ff;
const AMBER: u32 = 0x00d2_9900;
const RED: u32 = 0x00f8_5161;
const COMPACT_WIDTH: f32 = 920.0;
const CONVERSATION_COLUMN_WIDTH: f32 = 820.0;
const UPDATE_POLL_INTERVAL: Duration = Duration::from_hours(6);
const UPDATE_POLL_JITTER: Duration = Duration::from_mins(30);
/// Vertical budget for the detail blocks inside one tool entry.
const ENTRY_DETAIL_BUDGET: f32 = 480.0;
/// Measured thumb geometry for a scrollable region.
struct ScrollbarGeometry {
    track_top: gpui::Pixels,
    track: f32,
    thumb_top: f32,
    thumb: f32,
    usable: f32,
    scrollable: f32,
}

/// A scrollbar drag in progress.
#[derive(Clone, Debug)]
struct ScrollbarDrag {
    /// Which scrollable region is being dragged.
    id: String,
    /// Distance from the top of the thumb to the grab point, so the thumb
    /// keeps its position under the pointer instead of recentring on it.
    grab_offset: f32,
}

/// Smallest usable scrollbar thumb.
const MIN_THUMB_HEIGHT: f32 = 24.0;
/// Scrollbar track width; wide enough to aim at without crowding content.
const SCROLLBAR_WIDTH: f32 = 14.0;
/// Thumb width, leaving a small margin inside the track.
const THUMB_WIDTH: f32 = 10.0;
/// Scrollbar id for the conversation itself.
const TRANSCRIPT_SCROLL_ID: &str = "transcript";
/// The command never takes more than a third of that budget, so output — the
/// part worth reading — always gets the majority.
const COMMAND_BLOCK_HEIGHT: f32 = ENTRY_DETAIL_BUDGET / 3.0;

/// Desktop-environment application identifier. On Wayland this becomes the
/// `xdg_toplevel` app ID and on X11 the `WM_CLASS`; both are used to match the
/// installed `com.constructomech.gcabb.desktop` entry that supplies the icon.
const APP_ID: &str = "com.constructomech.gcabb";

actions!(gcabb, [DismissPopup, FocusNext, FocusPrevious]);

const MARKDOWN_STRONG: u8 = 1;
const MARKDOWN_EMPHASIS: u8 = 1 << 1;
const MARKDOWN_STRIKETHROUGH: u8 = 1 << 2;

#[derive(Clone, Default)]
struct MarkdownInlineStyle {
    marks: u8,
    link: Option<String>,
}

impl MarkdownInlineStyle {
    fn has(&self, mark: u8) -> bool {
        self.marks & mark != 0
    }
}

fn safe_markdown_url(target: &str) -> Option<String> {
    let target = target.trim();
    let lowercase = target.to_ascii_lowercase();
    (lowercase.starts_with("https://")
        || lowercase.starts_with("http://")
        || lowercase.starts_with("mailto:"))
    .then(|| target.to_owned())
}

/// An image shown full size over the session.
#[derive(Clone)]
struct ImagePreview {
    title: String,
    source: PreviewSource,
}

/// Where the pixels for a preview come from.
///
/// A file on disk is loaded by path so the bytes are not held twice. A pasted
/// image has no file yet, so its decoded bytes are kept until the runtime
/// echoes back a path for it.
#[derive(Clone)]
enum PreviewSource {
    Path(PathBuf),
    Bytes(std::sync::Arc<gpui::Image>),
}

/// Build a preview for an attachment staged in the composer.
fn draft_preview(attachment: &PromptAttachment) -> Option<ImagePreview> {
    if !attachment.is_image() {
        return None;
    }
    let title = attachment.display_name().to_owned();
    if let Some(path) = attachment.path() {
        return Some(ImagePreview {
            title,
            source: PreviewSource::Path(PathBuf::from(path)),
        });
    }
    Some(ImagePreview {
        title,
        source: PreviewSource::Bytes(std::sync::Arc::new(gpui::Image {
            format: image_format_for(attachment.mime_type()?)?,
            bytes: attachment.image_bytes()?,
            id: 0,
        })),
    })
}

/// Map a MIME type onto the format gpui needs to decode it.
fn image_format_for(mime_type: &str) -> Option<gpui::ImageFormat> {
    match mime_type {
        "image/png" => Some(gpui::ImageFormat::Png),
        "image/jpeg" => Some(gpui::ImageFormat::Jpeg),
        "image/webp" => Some(gpui::ImageFormat::Webp),
        "image/gif" => Some(gpui::ImageFormat::Gif),
        "image/bmp" => Some(gpui::ImageFormat::Bmp),
        _ => None,
    }
}
enum ServiceUpdate {
    Ready {
        compatibility: ProviderCompatibility,
        projects: Vec<ProjectMetadata>,
        failures: Vec<RestoreFailure>,
    },
    SessionHydrated(SessionHandle),
    RestorationFinished(Vec<RestoreFailure>),
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
        attachments: Vec<PromptAttachment>,
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
    bootstrap: Option<BootstrapState>,
}

struct BootstrapState {
    projects: Vec<ProjectMetadata>,
    sessions: Vec<SessionMetadata>,
    selected_session: Option<String>,
}

impl AppService {
    #[allow(clippy::too_many_lines)]
    fn start(project_root: PathBuf, database_path: &Path) -> Self {
        let startup_started = Instant::now();
        let diagnostics = Arc::new(TracingDiagnostics);
        let storage_started = Instant::now();
        let storage = match Storage::open(database_path) {
            Ok(storage) => Arc::new(storage),
            Err(error) => {
                return Self::failed(format!(
                    "failed to open {}: {error}",
                    database_path.display()
                ));
            }
        };
        let storage_ms = elapsed_millis(storage_started);
        let bootstrap = BootstrapState {
            projects: storage.list_projects().unwrap_or_else(|error| {
                tracing::error!(%error, "failed to list bootstrap projects");
                Vec::new()
            }),
            sessions: storage.list_sessions().unwrap_or_else(|error| {
                tracing::error!(%error, "failed to list bootstrap sessions");
                Vec::new()
            }),
            selected_session: storage.selected_session().unwrap_or(None),
        };
        diagnostics.record(DiagnosticEvent {
            timestamp: timestamp(),
            category: "desktop_startup".to_owned(),
            operation: "bootstrap".to_owned(),
            elapsed_ms: Some(elapsed_millis(startup_started)),
            session_id: bootstrap.selected_session.clone(),
            success: true,
            details: serde_json::json!({
                "storageMs": storage_ms,
                "projectCount": bootstrap.projects.len(),
                "sessionCount": bootstrap.sessions.len()
            }),
        });
        let preferred_session = bootstrap
            .selected_session
            .as_ref()
            .filter(|id| bootstrap.sessions.iter().any(|session| &session.id == *id))
            .cloned()
            .or_else(|| bootstrap.sessions.first().map(|session| session.id.clone()));
        let (update_tx, updates) = channel();
        let (commands, command_rx) = channel();
        let (stopped_tx, stopped) = channel();
        thread::Builder::new()
            .name("gcabb-services".to_owned())
            .spawn(move || {
                let runtime_started = Instant::now();
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
                let runtime_ms = elapsed_millis(runtime_started);
                let provider = Arc::new(CopilotProvider::new(
                    project_root.clone(),
                    diagnostics.clone(),
                ));
                let manager = Arc::new(SessionManager::new(provider, storage, diagnostics.clone()));
                let session_roots = SessionRoots {
                    worktrees: Some(worktrees_root()),
                    attachments: attachments_directory(),
                    runtime_state: runtime_state_root(),
                };
                // Projects are configured by the user, not inferred from the
                // launch directory. Auto-registering the launch repository
                // would silently re-add a project the user had removed.

                // Fold projects and sessions recorded by earlier builds, which
                // registered one project per worktree, into their repository.
                let adoption_started = Instant::now();
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
                let adoption_ms = elapsed_millis(adoption_started);

                let manager_started = Instant::now();
                let mut restoration_task = None;
                match runtime.block_on(manager.start_preferred_session(
                    preferred_session.as_deref(),
                    |handle| {
                        let _ = update_tx.send(ServiceUpdate::SessionHydrated(handle));
                    },
                )) {
                    Ok((compatibility, report, remaining)) => {
                        let manager_ms = elapsed_millis(manager_started);
                        let metadata_started = Instant::now();
                        let projects = manager.projects().unwrap_or_else(|error| {
                            tracing::error!(%error, "failed to list projects");
                            Vec::new()
                        });
                        let metadata_ms = elapsed_millis(metadata_started);
                        diagnostics.record(DiagnosticEvent {
                            timestamp: timestamp(),
                            category: "desktop_startup".to_owned(),
                            operation: "ready".to_owned(),
                            elapsed_ms: Some(elapsed_millis(startup_started)),
                            session_id: preferred_session.clone(),
                            success: true,
                            details: serde_json::json!({
                                "runtimeMs": runtime_ms,
                                "storageMs": storage_ms,
                                "adoptionMs": adoption_ms,
                                "managerMs": manager_ms,
                                "metadataMs": metadata_ms,
                                "projectCount": projects.len(),
                                "restoredSessions": report.restored.len(),
                                "failedSessions": report.failed.len(),
                                "remainingSessions": remaining.len()
                            }),
                        });
                        let _ = update_tx.send(ServiceUpdate::Ready {
                            compatibility,
                            projects,
                            failures: report.failed,
                        });
                        let background_manager = manager.clone();
                        let background_updates = update_tx.clone();
                        restoration_task = Some(runtime.spawn(async move {
                            let report = background_manager
                                .restore_remaining_sessions(remaining, |handle| {
                                    let _ = background_updates
                                        .send(ServiceUpdate::SessionHydrated(handle));
                                })
                                .await;
                            let _ = background_updates
                                .send(ServiceUpdate::RestorationFinished(report.failed));
                        }));
                    }
                    Err(error) => {
                        let _ = update_tx.send(ServiceUpdate::Failed(format!(
                            "Copilot provider startup failed: {error}"
                        )));
                    }
                }

                while let Ok(command) = command_rx.recv() {
                    if matches!(command, ServiceCommand::Stop) {
                        if let Some(task) = restoration_task.take() {
                            let _ = runtime.block_on(task);
                        }
                        let _ = runtime.block_on(manager.stop());
                        break;
                    }
                    // Project changes publish a project list rather than a
                    // session, so they are handled before the session commands.
                    match command {
                        ServiceCommand::DeleteSession { app_session_id } => {
                            match runtime
                                .block_on(manager.delete_session(&app_session_id, &session_roots))
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
                            let naming_prompt = match &command {
                                ServiceCommand::Submit {
                                    app_session_id: None,
                                    prompt,
                                    ..
                                } => Some(prompt.clone()),
                                _ => None,
                            };
                            match runtime.block_on(handle_service_command(
                                &manager,
                                command,
                                &session_roots.worktrees.clone().unwrap_or_default(),
                            )) {
                                Ok(Some(handle)) => {
                                    if let Some(prompt) = naming_prompt {
                                        let manager = manager.clone();
                                        let session_id = handle.id().to_owned();
                                        runtime.spawn(async move {
                                            if let Err(error) = manager
                                                .generate_session_title(&session_id, &prompt)
                                                .await
                                            {
                                                tracing::warn!(
                                                    %error,
                                                    %session_id,
                                                    "session title generation failed"
                                                );
                                            }
                                        });
                                    }
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
            bootstrap: Some(bootstrap),
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
            bootstrap: None,
        }
    }

    /// A service with no backing thread, plus the command receiver.
    ///
    /// View tests drive real UI code but must not start a Copilot provider, so
    /// commands are captured and asserted on instead of executed.
    #[cfg(test)]
    fn for_test() -> (Self, Receiver<ServiceCommand>) {
        let (service, commands, _updates) = Self::for_test_with_updates();
        (service, commands)
    }

    #[cfg(test)]
    fn for_test_with_updates() -> (Self, Receiver<ServiceCommand>, Sender<ServiceUpdate>) {
        let (update_tx, updates) = channel();
        let (commands, command_rx) = channel();
        let (_stopped_tx, stopped) = channel();
        (
            Self {
                updates,
                commands,
                stopped,
                bootstrap: None,
            },
            command_rx,
            update_tx,
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
            attachments,
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
                        title_source: TitleSource::Fallback,
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
                .send_with_attachments(prompt, attachments)
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
/// Where the runtime keeps per-session state, keyed by its own session id.
///
/// Deleting a session leaves this behind otherwise; one machine had 114 MB
/// across 69 directories for sessions that no longer existed.
fn runtime_state_root() -> Option<PathBuf> {
    let path = dirs::home_dir()?.join(".copilot").join("session-state");
    path.is_dir().then_some(path)
}

fn worktrees_root() -> PathBuf {
    data_directory().map_or_else(
        |_| PathBuf::from(".gcabb").join("worktrees"),
        |base| base.join("worktrees"),
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
    if title.is_empty() {
        "New session".to_owned()
    } else if title.chars().count() > 56 {
        title.chars().take(53).collect::<String>() + "..."
    } else {
        title
    }
}

struct SessionProjection {
    _handle: Option<SessionHandle>,
    receiver: Option<watch::Receiver<Arc<SessionSnapshot>>>,
    snapshot: Arc<SessionSnapshot>,
}

impl SessionProjection {
    fn new(handle: SessionHandle) -> Self {
        let receiver = handle.subscribe();
        let snapshot = receiver.borrow().clone();
        Self {
            _handle: Some(handle),
            receiver: Some(receiver),
            snapshot,
        }
    }

    fn bootstrap(metadata: SessionMetadata) -> Self {
        let mut snapshot = SessionSnapshot::new(metadata);
        snapshot.status = SessionStatus::Recovering;
        Self {
            _handle: None,
            receiver: None,
            snapshot: Arc::new(snapshot),
        }
    }

    fn id(&self) -> &str {
        &self.snapshot.metadata.id
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
enum StartupNavigation {
    Untouched,
    Changed,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SettingsVisibility {
    Closed,
    Open,
}

struct SessionMvpView {
    startup: StartupState,
    projects: Vec<ProjectMetadata>,
    sessions: Vec<SessionProjection>,
    selected_session: Option<String>,
    /// User navigation during startup wins over delayed bootstrap/restoration.
    startup_navigation: StartupNavigation,
    /// Repository grouping key for the sidebar.
    selected_project: PathBuf,
    /// Directory new sessions run in.
    workspace_root: PathBuf,
    /// Directory GCABB was launched from, used when no project is selected.
    launch_workspace: PathBuf,
    /// Working directory chats run in, since chats have no repository.
    chats_workspace: PathBuf,
    /// Where pasted images are written so they can be referenced by path.
    attachments_root: Option<PathBuf>,
    /// Whether the composer will start a chat rather than a project session.
    composing_chat: bool,
    /// Where the next project session will run.
    draft_location: SessionLocation,
    /// Files staged to travel with the next prompt.
    draft_attachments: Vec<PromptAttachment>,
    /// The image being shown full size, if any.
    image_preview: Option<ImagePreview>,
    /// Focus for the preview, so Escape reaches it however it was opened.
    image_preview_focus: FocusHandle,
    /// Branch currently checked out in the selected project, refreshed when
    /// the selection changes so the composer never runs git per frame.
    project_branch: Option<String>,
    /// Scroll position of the transcript.
    transcript_scroll: gpui::ScrollHandle,
    /// Scroll positions of the detail blocks inside tool entries, keyed by
    /// block id so each keeps its position across renders.
    detail_scrolls: RefCell<HashMap<String, gpui::ScrollHandle>>,
    /// Last rendered content length for each detail block, used to follow
    /// streaming shell output without resetting blocks the user scrolled up.
    detail_extents: RefCell<HashMap<String, usize>>,
    /// Scrollable extent the transcript last rendered with.
    ///
    /// Scrollbar geometry is only knowable after a layout pass, and the window
    /// now repaints only when something changed, so a static transcript would
    /// never come back to draw its scrollbar. Noticing the extent change asks
    /// for exactly one more frame.
    transcript_extent_px: f32,
    /// Scrollbar currently being dragged, if any.
    ///
    /// Tracked on the view rather than the thumb so a drag keeps working once
    /// the pointer leaves the narrow track, which is most of the time.
    dragging_scrollbar: Option<ScrollbarDrag>,
    /// Transcript length last auto-scrolled for, so the view follows new
    /// output without fighting a user who has scrolled up to read.
    transcript_extent: (String, usize, usize, usize, usize),
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
    /// What the update banner is showing.
    update_ui: UpdateUi,
    /// Background update worker, absent for developer builds that never update.
    update_service: Option<UpdateService>,
    settings_visibility: SettingsVisibility,
    _poll_task: gpui::Task<()>,
    _update_poll_task: gpui::Task<()>,
}

impl SessionMvpView {
    #[allow(clippy::too_many_lines)]
    fn new(
        service: AppService,
        project_root: PathBuf,
        branch: String,
        chats_workspace: PathBuf,
        attachments_root: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) -> Self {
        let AppService {
            updates,
            commands,
            stopped,
            bootstrap,
        } = service;
        let quit_commands = commands.clone();
        let stopped = Arc::new(Mutex::new(stopped));
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
        cx.subscribe(&composer, |view, _, event: &ImagesPasted, cx| {
            view.attach_pasted_images(&event.images, cx);
        })
        .detach();
        cx.observe(&composer, |_, _, cx| cx.notify()).detach();
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
                        let banner_changed = view.apply_update_events();
                        if updated || refreshed || banner_changed {
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        });

        // A developer build never updates itself, so it gets no worker and no
        // banner rather than a disabled one.
        let build = BuildStamp::current();
        let update_service = match (build.is_release(), data_directory()) {
            (true, Ok(data_dir)) => {
                let service = UpdateService::start(build, data_dir);
                service.request(UpdateRequest::Check { automatic: true });
                Some(service)
            }
            _ => None,
        };
        let periodic_update_delay = update_poll_delay();
        let update_poll_task = cx.spawn(async move |view, cx| {
            loop {
                cx.background_executor().timer(periodic_update_delay).await;
                if view
                    .update(cx, |view, _| {
                        view.request_update(UpdateRequest::Check { automatic: true });
                    })
                    .is_err()
                {
                    break;
                }
            }
        });

        let mut view = Self {
            startup: StartupState::Starting,
            projects: Vec::new(),
            sessions: Vec::new(),
            selected_session: None,
            startup_navigation: StartupNavigation::Untouched,
            selected_project: repository_root(&project_root),
            workspace_root: project_root.clone(),
            launch_workspace: project_root,
            chats_workspace,
            attachments_root,
            composing_chat: false,
            draft_location: SessionLocation::default(),
            draft_attachments: Vec::new(),
            image_preview: None,
            image_preview_focus: cx.focus_handle(),
            project_branch: None,
            transcript_scroll: gpui::ScrollHandle::new(),
            detail_scrolls: RefCell::new(HashMap::new()),
            detail_extents: RefCell::new(HashMap::new()),
            transcript_extent_px: 0.0,
            dragging_scrollbar: None,
            transcript_extent: (String::new(), 0, 0, 0, 0),
            restore_failures: Vec::new(),
            updates,
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
            update_ui: UpdateUi::default(),
            update_service,
            settings_visibility: SettingsVisibility::Closed,
            _poll_task: poll_task,
            _update_poll_task: update_poll_task,
        };
        if let Some(bootstrap) = bootstrap {
            view.apply_bootstrap(bootstrap);
        }
        view
    }

    /// Message, accent colour, and optional detail line for the update banner.
    fn update_banner_text(&self) -> Option<(String, u32, Option<String>)> {
        let (message, accent) = match &self.update_ui {
            UpdateUi::Hidden => return None,
            UpdateUi::Checking => ("Checking for updates…".to_owned(), MUTED),
            UpdateUi::Available { version, .. } => (format!("GCABB {version} is available"), BLUE),
            UpdateUi::Downloading { .. } => (
                self.update_ui.percent().map_or_else(
                    || "Downloading update…".to_owned(),
                    |percent| format!("Downloading update… {percent}%"),
                ),
                BLUE,
            ),
            UpdateUi::ReadyToRestart { version } => (
                format!("GCABB {version} is installed and starts on restart"),
                GREEN,
            ),
            UpdateUi::Failed(error) => (format!("Update failed: {error}"), RED),
        };

        // Release notes are shown as a short summary; the full text lives in
        // the GitHub Release, and a banner is the wrong place for a changelog.
        let summary = match &self.update_ui {
            UpdateUi::Available { notes, .. } => notes
                .lines()
                .find(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'))
                .map(|line| line.trim().to_owned()),
            _ => None,
        };

        Some((message, accent, summary))
    }

    /// The update banner, when there is something to say about an update.
    ///
    /// Returns `None` in the common case so an install with nothing to report
    /// spends no space on it.
    fn update_banner(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let (message, accent, summary) = self.update_banner_text()?;

        let banner = div()
            .id("update-banner")
            .debug_selector(|| "update-banner".to_owned())
            .accessibility_id("update-banner")
            .role(Role::Group)
            .aria_label("Update")
            .flex()
            .items_center()
            .gap_3()
            .w_full()
            .px_4()
            .py_2()
            .bg(rgb(ELEVATED))
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(
                div()
                    .w(px(8.0))
                    .h(px(8.0))
                    .rounded_full()
                    .bg(rgb(accent))
                    .flex_shrink_0(),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .child(div().text_sm().text_color(rgb(PRIMARY)).child(message))
                    .when_some(summary, |column, summary| {
                        column.child(
                            div()
                                .text_xs()
                                .text_color(rgb(MUTED))
                                .truncate()
                                .child(summary),
                        )
                    }),
            );

        let banner = match &self.update_ui {
            UpdateUi::Available { .. } => banner
                .child(action_button("Update", BLUE, cx, |view| {
                    view.request_update(UpdateRequest::Install);
                }))
                .child(action_button("Later", ELEVATED, cx, |view| {
                    view.request_update(UpdateRequest::Defer);
                })),
            UpdateUi::ReadyToRestart { version } => {
                banner.child(Self::restart_button(version.clone(), cx))
            }
            UpdateUi::Failed(_) => banner.child(action_button("Dismiss", ELEVATED, cx, |view| {
                view.update_ui = UpdateUi::Hidden;
            })),
            _ => banner,
        };

        Some(banner.into_any_element())
    }

    /// Button that starts the replacement build and closes this one.
    fn restart_button(version: String, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("update-restart")
            .debug_selector(|| "update-restart".to_owned())
            .accessibility_id("update-restart")
            .role(Role::Button)
            .aria_label("Restart")
            .focusable()
            .tab_stop(true)
            .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
            .px_4()
            .py_2()
            .rounded_md()
            .bg(rgb(GREEN))
            .text_color(rgb(BACKGROUND))
            .child("Restart")
            .hover(|style| style.opacity(0.85).cursor_pointer())
            .on_click(cx.listener(
                move |view, _, _, cx| match updates::restart_into_updated_build(&version) {
                    // The replacement is running, so this process can go.
                    Ok(()) => cx.quit(),
                    Err(error) => {
                        view.update_ui = UpdateUi::Failed(error);
                        cx.notify();
                    }
                },
            ))
    }

    /// Forwards a request to the update worker.
    fn request_update(&mut self, request: UpdateRequest) {
        if let Some(service) = self.update_service.as_ref() {
            service.request(request);
        }
    }

    fn settings_check_button(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let updates_available = self.update_service.is_some();
        let checking = self.update_ui == UpdateUi::Checking;
        let check_label = if checking {
            "Checking…"
        } else if updates_available {
            "Check for updates"
        } else {
            "Unavailable in development builds"
        };

        div()
            .id("settings-check-updates")
            .accessibility_id("settings-check-updates")
            .role(Role::Button)
            .aria_label(check_label)
            .focusable()
            .tab_stop(updates_available && !checking)
            .px_4()
            .py_2()
            .rounded_md()
            .border_1()
            .border_color(rgb(BORDER))
            .text_sm()
            .text_color(if updates_available && !checking {
                rgb(PRIMARY)
            } else {
                rgb(MUTED)
            })
            .child(check_label)
            .when(updates_available && !checking, |button| {
                button
                    .hover(|style| style.bg(rgb(ELEVATED)).cursor_pointer())
                    .on_click(cx.listener(|view, _, _, cx| {
                        view.request_update(UpdateRequest::Check { automatic: false });
                        cx.notify();
                    }))
            })
            .into_any_element()
    }

    fn settings_close_button(cx: &mut Context<Self>) -> gpui::AnyElement {
        div()
            .id("settings-close")
            .accessibility_id("settings-close")
            .role(Role::Button)
            .aria_label("Close settings")
            .focusable()
            .tab_stop(true)
            .px_4()
            .py_2()
            .rounded_md()
            .bg(rgb(ELEVATED))
            .child("Close")
            .hover(|style| style.opacity(0.85).cursor_pointer())
            .on_click(cx.listener(|view, _, _, cx| {
                view.settings_visibility = SettingsVisibility::Closed;
                cx.notify();
            }))
            .into_any_element()
    }

    fn settings_dialog(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        if self.settings_visibility != SettingsVisibility::Open {
            return None;
        }
        let version = BuildStamp::current().version.to_string();

        Some(
            div()
                .id("settings-dialog")
                .accessibility_id("settings-dialog")
                .role(Role::Dialog)
                .aria_label("Settings")
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(gpui::rgba(0x0000_00a8))
                .child(
                    div()
                        .id("settings-panel")
                        .w(px(460.0))
                        .flex()
                        .flex_col()
                        .gap_4()
                        .p_5()
                        .rounded_lg()
                        .bg(rgb(PANEL))
                        .border_1()
                        .border_color(rgb(BORDER))
                        .shadow_lg()
                        .child(
                            div()
                                .id("settings-heading")
                                .role(Role::Heading)
                                .aria_level(2)
                                .aria_label("Settings")
                                .text_xl()
                                .font_weight(gpui::FontWeight::BOLD)
                                .child("Settings"),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .gap_4()
                                .child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .gap_1()
                                        .child(div().child("Updates"))
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(rgb(MUTED))
                                                .child(format!("Current version: {version}")),
                                        ),
                                )
                                .child(self.settings_check_button(cx)),
                        )
                        .child(
                            div()
                                .flex()
                                .justify_end()
                                .child(Self::settings_close_button(cx)),
                        ),
                ),
        )
    }

    /// Drains pending update-worker events into the banner.
    fn apply_update_events(&mut self) -> bool {
        // Destructured so the worker and the banner are borrowed as separate
        // fields rather than through one borrow of `self`.
        let Self {
            update_service,
            update_ui,
            ..
        } = self;
        update_service
            .as_ref()
            .is_some_and(|service| service.drain(update_ui))
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
                    failures,
                } => {
                    self.startup = StartupState::Ready(compatibility);
                    self.projects = projects;
                    self.apply_restore_failures(failures);
                }
                ServiceUpdate::SessionHydrated(handle) => {
                    self.upsert_hydrated_session(handle);
                }
                ServiceUpdate::RestorationFinished(failures) => {
                    self.apply_restore_failures(failures);
                }
                ServiceUpdate::SessionAdded(handle) => {
                    let id = handle.id().to_owned();
                    self.upsert_hydrated_session(handle);
                    self.selected_session = Some(id);
                }
                ServiceUpdate::SessionsDiscovered(handles) => {
                    for handle in handles {
                        self.upsert_hydrated_session(handle);
                    }
                }
                ServiceUpdate::ProjectsChanged { projects, selected } => {
                    self.apply_projects_changed(projects, selected, cx);
                }
                ServiceUpdate::SessionDeleted(id) => {
                    self.sessions.retain(|session| session.id() != id);
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

    fn apply_bootstrap(&mut self, bootstrap: BootstrapState) {
        self.projects = bootstrap.projects;
        self.sessions = bootstrap
            .sessions
            .into_iter()
            .map(SessionProjection::bootstrap)
            .collect();
        if self.startup_navigation == StartupNavigation::Untouched {
            self.selected_session = bootstrap
                .selected_session
                .filter(|id| self.sessions.iter().any(|session| session.id() == id))
                .or_else(|| self.sessions.first().map(|session| session.id().to_owned()));
            self.adopt_selected_session_location();
        }
        if self.projects.is_empty() && self.selected_session.is_none() {
            self.composing_chat = true;
        }
    }

    fn apply_restore_failures(&mut self, failures: Vec<RestoreFailure>) {
        for failure in &failures {
            if let Some(session) = self
                .sessions
                .iter_mut()
                .find(|session| session.id() == failure.app_session_id)
            {
                let mut snapshot = (*session.snapshot).clone();
                snapshot.status = SessionStatus::Failed;
                snapshot.last_error = Some(failure.error.clone());
                session.snapshot = Arc::new(snapshot);
            }
        }
        self.restore_failures.extend(failures);
    }

    fn upsert_hydrated_session(&mut self, handle: SessionHandle) {
        let id = handle.id().to_owned();
        if let Some(index) = self.sessions.iter().position(|session| session.id() == id) {
            self.sessions[index] = SessionProjection::new(handle);
        } else {
            self.sessions.insert(0, SessionProjection::new(handle));
        }
        if self.startup_navigation == StartupNavigation::Untouched {
            if self.selected_session.is_none() {
                self.selected_session = Some(id);
            }
            self.adopt_selected_session_location();
        }
    }

    fn adopt_selected_session_location(&mut self) {
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
        self.startup_navigation = StartupNavigation::Changed;
        let _ = self.commands.send(ServiceCommand::Select {
            app_session_id: None,
        });
    }

    /// Pulls any changed session snapshots, returning whether one actually moved.
    fn refresh_snapshots(&mut self) -> bool {
        let mut changed = false;
        for projection in &mut self.sessions {
            if projection
                .receiver
                .as_ref()
                .is_some_and(|receiver| receiver.has_changed().unwrap_or(false))
            {
                projection.snapshot = projection
                    .receiver
                    .as_mut()
                    .expect("changed receiver is present")
                    .borrow_and_update()
                    .clone();
                changed = true;
            }
        }
        changed
    }

    fn selected(&self) -> Option<&SessionProjection> {
        let id = self.selected_session.as_deref()?;
        self.sessions.iter().find(|session| session.id() == id)
    }

    fn submit_prompt(&mut self, prompt: String) {
        let attachments = std::mem::take(&mut self.draft_attachments);
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
            attachments,
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

    /// Ask for another frame when the transcript's scrollable extent changes.
    ///
    /// The scrollbar can only be sized once a layout pass has measured the
    /// content, so the frame that first grows the transcript cannot draw it.
    fn note_transcript_extent(&mut self, cx: &mut Context<Self>) {
        let extent = f32::from(self.transcript_scroll.max_offset().y);
        if (extent - self.transcript_extent_px).abs() > f32::EPSILON {
            self.transcript_extent_px = extent;
            cx.notify();
        }
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
            session.snapshot.tool_activity.invocations.len(),
            session
                .snapshot
                .tool_activity
                .invocations
                .iter()
                .map(|invocation| invocation.output.len())
                .sum(),
        );
        if extent == self.transcript_extent {
            return;
        }
        let switched_session = extent.0 != self.transcript_extent.0;
        let grew = extent.1 > self.transcript_extent.1
            || extent.2 > self.transcript_extent.2
            || extent.3 > self.transcript_extent.3
            || extent.4 > self.transcript_extent.4;
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
        self.startup_navigation = StartupNavigation::Changed;
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
            app_session_id: session.id().to_owned(),
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
        self.startup_navigation = StartupNavigation::Changed;
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
        self.startup_navigation = StartupNavigation::Changed;
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
            .map(|session| session.id().to_owned());
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
                .find(|session| session.id() == app_session_id)
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
        self.startup_navigation = StartupNavigation::Changed;
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

    /// Chips for the files staged on the next prompt, each removable.
    ///
    /// Returns nothing when there is nothing attached so the composer keeps
    /// its usual shape.
    fn attachment_strip(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        if self.draft_attachments.is_empty() {
            return None;
        }
        let chips: Vec<_> = self
            .draft_attachments
            .iter()
            .map(|attachment| {
                let identity = attachment.identity();
                let label = attachment.display_name().to_owned();
                let preview_identity = identity.clone();
                let preview_label = label.clone();
                let remove_identity = identity.clone();
                let remove_label = label.clone();
                let icon = if attachment.is_image() { "IMG" } else { "FILE" };
                let preview = draft_preview(attachment);
                let content = div()
                    .id(SharedString::from(format!(
                        "preview-attachment-{preview_identity}"
                    )))
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().text_xs().text_color(rgb(MUTED)).child(icon))
                    .child(div().text_xs().text_color(rgb(PRIMARY)).child(label))
                    .when_some(preview, |content, preview| {
                        content
                            .accessibility_id(format!("preview-attachment-{preview_identity}"))
                            .role(Role::Button)
                            .aria_label(format!("Preview {preview_label}"))
                            .focusable()
                            .tab_stop(true)
                            .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                            .hover(gpui::Styled::cursor_pointer)
                            .on_click(cx.listener(move |view, _, window, cx| {
                                view.open_image_preview(preview.clone(), window, cx);
                            }))
                    });
                div()
                    .id(SharedString::from(format!("attachment-{identity}")))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .bg(rgb(SUBTLE))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .child(content)
                    .child(
                        div()
                            .id(SharedString::from(format!(
                                "remove-attachment-{remove_identity}"
                            )))
                            .accessibility_id(format!("remove-attachment-{remove_identity}"))
                            .role(Role::Button)
                            .aria_label(format!("Remove {remove_label}"))
                            .focusable()
                            .tab_stop(true)
                            .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child("x")
                            .hover(|style| style.text_color(rgb(PRIMARY)).cursor_pointer())
                            .on_click(cx.listener(move |view, _, _, cx| {
                                view.remove_attachment(&remove_identity, cx);
                            })),
                    )
            })
            .collect();
        Some(
            div()
                .id("attachment-strip")
                .accessibility_id("attachment-strip")
                .debug_selector(|| "attachment-strip".to_owned())
                .flex()
                .flex_wrap()
                .gap_2()
                .px_3()
                .pb_2()
                .children(chips),
        )
    }

    /// Show an attachment full size.
    ///
    /// Takes focus so Escape closes it. A click on a chip leaves focus
    /// wherever it was, which left Escape dead exactly when a user would
    /// reach for it.
    fn open_image_preview(
        &mut self,
        preview: ImagePreview,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.image_preview = Some(preview);
        window.focus(&self.image_preview_focus, cx);
        cx.notify();
    }

    /// Close the preview, if one is open.
    fn dismiss_image_preview(&mut self, cx: &mut Context<Self>) {
        if self.image_preview.take().is_some() {
            cx.notify();
        }
    }

    /// The full-size image overlay.
    fn image_preview_overlay(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let preview = self.image_preview.as_ref()?;
        let title = preview.title.clone();
        let image: gpui::Img = match &preview.source {
            PreviewSource::Path(path) => gpui::img(path.clone()),
            PreviewSource::Bytes(image) => gpui::img(image.clone()),
        };
        Some(
            div()
                .id("image-preview")
                .accessibility_id("image-preview")
                .track_focus(&self.image_preview_focus)
                .debug_selector(|| "image-preview".to_owned())
                .role(Role::Dialog)
                .aria_label(format!("Preview of {title}"))
                .absolute()
                .inset_0()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_3()
                .bg(gpui::rgba(0x0000_00d8))
                // Anywhere outside the picture closes it, which is what a
                // lightbox trains people to expect.
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|view, _, _, cx| view.dismiss_image_preview(cx)),
                )
                .child(
                    div()
                        .id("image-preview-close")
                        .accessibility_id("image-preview-close")
                        .role(Role::Button)
                        .aria_label("Close image preview")
                        .focusable()
                        .tab_stop(true)
                        .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                        .px_3()
                        .py_1()
                        .rounded_md()
                        .bg(rgb(PANEL))
                        .text_sm()
                        .text_color(rgb(PRIMARY))
                        .child("Close")
                        .hover(|style| style.bg(rgb(ELEVATED)).cursor_pointer())
                        .on_click(cx.listener(|view, _, _, cx| {
                            view.dismiss_image_preview(cx);
                        })),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(rgb(PRIMARY))
                        .child(title.clone()),
                )
                .child(
                    div()
                        .id("image-preview-frame")
                        .max_w(px(1100.0))
                        .max_h(px(760.0))
                        .p_2()
                        .rounded_lg()
                        .bg(rgb(PANEL))
                        .border_1()
                        .border_color(rgb(BORDER))
                        .shadow_lg()
                        // Clicking the picture itself must not dismiss it.
                        .occlude()
                        .child(image.max_w(px(1080.0)).max_h(px(720.0))),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(MUTED))
                        .child("Click anywhere or press Escape to close"),
                ),
        )
    }

    /// Stage images pasted into the composer.
    ///
    /// A pasted screenshot has no path, so it is carried as bytes. Each paste
    /// is a distinct attachment: someone who pastes twice meant two images.
    fn attach_pasted_images(&mut self, images: &[PastedImage], cx: &mut Context<Self>) {
        let directory = self.attachments_root.clone();
        for image in images {
            let index = self.draft_attachments.len() + 1;
            // Written to disk and sent as a file, matching what a picked or
            // dropped image does. Sending bytes inline instead meant the
            // runtime echoed back a blob with no path, so the transcript could
            // never show the picture again -- and a copy of those bytes was
            // persisted in the event log and in every later snapshot.
            let attachment = directory
                .as_deref()
                .and_then(|directory| {
                    write_pasted_image(directory, &image.bytes, &image.mime_type, index)
                })
                .unwrap_or_else(|| {
                    PromptAttachment::from_image_bytes(&image.bytes, image.mime_type.clone(), index)
                });
            self.draft_attachments.push(attachment);
        }
        cx.notify();
    }

    /// Stage files dropped onto the composer.
    fn attach_dropped_paths(&mut self, paths: &[PathBuf], cx: &mut Context<Self>) {
        for path in paths {
            let attachment = PromptAttachment::from_path(path);
            if !self
                .draft_attachments
                .iter()
                .any(|existing| existing.identity() == attachment.identity())
            {
                self.draft_attachments.push(attachment);
            }
        }
        cx.notify();
    }

    /// Open a file chooser and attach what the user picks.
    ///
    /// Screenshots are the primary way interface defects get reported, so this
    /// is the difference between a session that can work on the UI and one
    /// that cannot.
    fn pick_attachments(cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: None,
        });
        cx.spawn(async move |view, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let _ = view.update(cx, |view, cx| {
                for path in paths {
                    let attachment = PromptAttachment::from_path(&path);
                    if !view
                        .draft_attachments
                        .iter()
                        .any(|existing| existing.identity() == attachment.identity())
                    {
                        view.draft_attachments.push(attachment);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Drop an attachment the user changed their mind about.
    fn remove_attachment(&mut self, identity: &str, cx: &mut Context<Self>) {
        self.draft_attachments
            .retain(|attachment| attachment.identity() != identity);
        cx.notify();
    }

    fn submit_composer(&mut self, cx: &mut Context<Self>) {
        let prompt = self.composer.read(cx).value();
        let prompt = prompt.trim();
        // An attachment alone is a complete message; a screenshot often says
        // everything the user wants to say.
        if !prompt.is_empty() || !self.draft_attachments.is_empty() {
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
                let id = session.id().to_owned();
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
                let id = session.id().to_owned();
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
                            .id("settings-button")
                            .accessibility_id("settings-button")
                            .role(Role::Button)
                            .aria_label("Settings")
                            .focusable()
                            .tab_stop(true)
                            .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .text_color(rgb(MUTED))
                            .child("Settings")
                            .hover(|style| style.bg(rgb(ELEVATED)).cursor_pointer())
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.settings_visibility = SettingsVisibility::Open;
                                cx.notify();
                            })),
                    ),
            )
    }

    /// The scroll handle behind a scrollbar id.
    fn scroll_handle(&self, id: &str) -> Option<gpui::ScrollHandle> {
        if id == TRANSCRIPT_SCROLL_ID {
            return Some(self.transcript_scroll.clone());
        }
        self.detail_scrolls.borrow().get(id).cloned()
    }

    /// Move a scrollable region so the pointer position maps to a position in
    /// its content.
    ///
    /// The handle reports its own viewport bounds in window coordinates, which
    /// is what lets a thumb anywhere on screen be dragged without the element
    /// having to measure itself.
    fn drag_scrollbar_to(&self, id: &str, pointer_y: gpui::Pixels, grab_offset: f32) {
        let Some(handle) = self.scroll_handle(id) else {
            return;
        };
        let Some(geometry) = Self::scrollbar_geometry(&handle) else {
            return;
        };
        let local = f32::from(pointer_y - geometry.track_top) - grab_offset;
        let fraction = (local / geometry.usable).clamp(0.0, 1.0);
        handle.set_offset(gpui::point(
            handle.offset().x,
            px(-(fraction * geometry.scrollable)),
        ));
    }

    /// Where a scrollable region's thumb currently sits.
    fn scrollbar_geometry(handle: &gpui::ScrollHandle) -> Option<ScrollbarGeometry> {
        let bounds = handle.bounds();
        let track = f32::from(bounds.size.height);
        let scrollable = f32::from(handle.max_offset().y);
        if track <= 0.0 || scrollable <= 0.0 {
            return None;
        }
        let thumb = (track * (track / (track + scrollable))).max(MIN_THUMB_HEIGHT);
        let usable = (track - thumb).max(1.0);
        let scrolled = (-f32::from(handle.offset().y) / scrollable).clamp(0.0, 1.0);
        Some(ScrollbarGeometry {
            track_top: bounds.origin.y,
            track,
            thumb_top: scrolled * usable,
            thumb,
            usable,
            scrollable,
        })
    }

    /// Begin a scrollbar drag, remembering where the thumb was grabbed.
    ///
    /// Pressing the track jumps the thumb under the pointer; pressing the
    /// thumb keeps it where it is so the content does not lurch on grab.
    fn begin_scrollbar_drag(&mut self, id: &str, pointer_y: gpui::Pixels) {
        let Some(handle) = self.scroll_handle(id) else {
            return;
        };
        let grab_offset = Self::scrollbar_geometry(&handle).map_or(0.0, |geometry| {
            let local = f32::from(pointer_y - geometry.track_top);
            let within_thumb =
                local >= geometry.thumb_top && local <= geometry.thumb_top + geometry.thumb;
            if within_thumb {
                local - geometry.thumb_top
            } else {
                geometry.thumb / 2.0
            }
        });
        self.dragging_scrollbar = Some(ScrollbarDrag {
            id: id.to_owned(),
            grab_offset,
        });
        self.drag_scrollbar_to(id, pointer_y, grab_offset);
    }

    /// A scrollbar for a scrollable region, shown while the pointer is over it.
    ///
    /// GPUI has no scrollbar element, so this draws the track and thumb and
    /// wires the drag itself.
    fn scrollbar(
        id: &str,
        handle: &gpui::ScrollHandle,
        group: SharedString,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        // Drawn from the same geometry the drag hit-tests against. Computing
        // the two separately let them disagree — different clamps, and one
        // measured against the track while the other measured against the
        // viewport — so a press on the visible thumb was classified as a press
        // on bare track and jumped the content instead of grabbing.
        let geometry = Self::scrollbar_geometry(handle)?;
        let track_id = id.to_owned();
        let thumb_id = id.to_owned();

        Some(
            div()
                .id(SharedString::from(format!("{id}-scrollbar")))
                .debug_selector(|| "scrollbar".to_owned())
                .occlude()
                .absolute()
                .top_0()
                .right_0()
                .w(px(SCROLLBAR_WIDTH))
                .h(px(geometry.track))
                .opacity(0.0)
                .group_hover(group, |style| style.opacity(1.0))
                // Pressing bare track jumps the thumb there and starts a drag.
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |view, event: &gpui::MouseDownEvent, _, cx| {
                        view.begin_scrollbar_drag(&track_id, event.position.y);
                        cx.notify();
                    }),
                )
                // The track occludes what is behind it, so a release over the
                // track never reaches the window handler.
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|view, _, _, cx| {
                        view.dragging_scrollbar = None;
                        cx.notify();
                    }),
                )
                .child(
                    div()
                        // The thumb sits above the track and would otherwise
                        // swallow presses meant for it, so it carries the same
                        // handlers rather than relying on the track's.
                        .id(SharedString::from(format!("{id}-thumb")))
                        .debug_selector(|| "scrollbar-thumb".to_owned())
                        .absolute()
                        .top(px(geometry.thumb_top))
                        .right(px(2.0))
                        .w(px(THUMB_WIDTH))
                        .h(px(geometry.thumb))
                        .rounded_full()
                        .bg(rgb(BORDER))
                        .hover(|style| style.bg(rgb(MUTED)).cursor_pointer())
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |view, event: &gpui::MouseDownEvent, _, cx| {
                                view.begin_scrollbar_drag(&thumb_id, event.position.y);
                                cx.notify();
                            }),
                        )
                        .on_mouse_up(
                            MouseButton::Left,
                            cx.listener(|view, _, _, cx| {
                                view.dragging_scrollbar = None;
                                cx.notify();
                            }),
                        ),
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
    fn detail_block(
        &self,
        id: &str,
        content: String,
        max_height: f32,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let handle = self
            .detail_scrolls
            .borrow_mut()
            .entry(id.to_owned())
            .or_default()
            .clone();
        let previous_extent = self
            .detail_extents
            .borrow_mut()
            .insert(id.to_owned(), content.len());
        let at_tail = f32::from(handle.max_offset().y) + f32::from(handle.offset().y) <= 1.0;
        if previous_extent.is_none_or(|previous| content.len() > previous) && at_tail {
            handle.scroll_to_bottom();
        }

        let group = SharedString::from(format!("scroll-{id}"));
        let scrollbar = Self::scrollbar(id, &handle, group.clone(), cx);

        div()
            .id(SharedString::from(format!("{id}-frame")))
            .group(group)
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
                    .min_w_0()
                    .overflow_x_scroll()
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
            .children(scrollbar)
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
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (status, status_color) = match invocation.state {
            app_model::InvocationState::Running => ("running", GREEN),
            app_model::InvocationState::Succeeded => ("done", MUTED),
            app_model::InvocationState::Failed => ("failed", RED),
            app_model::InvocationState::Cancelled => ("cancelled", MUTED),
        };
        let summary = invocation.summary_line();
        let detail = invocation.multiline_summary();
        let verb = invocation.verb();
        let label = format!("{verb} {summary}");
        let diff = invocation.diff().map(str::to_owned);
        let error = invocation.error_message.clone();
        let output_error = invocation
            .output_load_error
            .clone()
            .or_else(|| invocation.output_error.clone());
        // Command output is the tail, since the interesting part is the end.
        // The block scrolls, so it can hold considerably more than the
        // terminals panel preview.
        let output = (matches!(
            invocation.class,
            app_model::ToolClass::Shell | app_model::ToolClass::ShellControl
        ) && !invocation.output.is_empty())
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
                    app_model::InvocationState::Succeeded
                    | app_model::InvocationState::Cancelled => MUTED,
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
                    .debug_selector(|| "tool-card".to_owned())
                    .w_full()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .bg(rgb(SUBTLE))
                    .overflow_hidden()
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
                    .when_some(output_error, |entry, error| {
                        entry.child(
                            div()
                                .id(SharedString::from(format!(
                                    "tool-output-error-{}",
                                    invocation.call_id
                                )))
                                .role(Role::Alert)
                                .text_xs()
                                .text_color(rgb(RED))
                                .child(format!("Output unavailable: {error}")),
                        )
                    })
                    .when_some(detail, |entry, detail| {
                        entry.child(self.detail_block(
                            &format!("tool-detail-{}", invocation.call_id),
                            detail,
                            COMMAND_BLOCK_HEIGHT,
                            cx,
                        ))
                    })
                    .when_some(diff, |entry, diff| {
                        entry.child(self.detail_block(
                            &format!("tool-diff-{}", invocation.call_id),
                            diff,
                            ENTRY_DETAIL_BUDGET - COMMAND_BLOCK_HEIGHT,
                            cx,
                        ))
                    })
                    .when_some(output, |entry, output| {
                        entry.child(self.detail_block(
                            &format!("tool-output-{}", invocation.call_id),
                            output,
                            ENTRY_DETAIL_BUDGET - COMMAND_BLOCK_HEIGHT,
                            cx,
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
    /// Chips for what a message was sent with, clickable when previewable.
    fn message_attachment_chips(
        message: &app_model::TranscriptMessage,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        message
            .attachments
            .iter()
            .enumerate()
            .map(|(index, attachment)| {
                let accessible_id = format!("message-attachment-{}-{index}", message.id);
                let accessible_label = attachment.display_name.clone();
                // Only an image backed by a file the runtime kept can be
                // shown; a name alone is not enough to load pixels.
                let preview = attachment
                    .is_image
                    .then(|| attachment.path.clone())
                    .flatten()
                    .map(|path| ImagePreview {
                        title: attachment.display_name.clone(),
                        source: PreviewSource::Path(PathBuf::from(path)),
                    });
                div()
                    .id(SharedString::from(accessible_id.clone()))
                    .debug_selector(|| "message-attachment".to_owned())
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .bg(rgb(SUBTLE))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .when_some(preview, |chip, preview| {
                        chip.accessibility_id(accessible_id)
                            .role(Role::Button)
                            .aria_label(format!("Preview {accessible_label}"))
                            .focusable()
                            .tab_stop(true)
                            .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                            .hover(|style| style.border_color(rgb(BLUE)).cursor_pointer())
                            .on_click(cx.listener(move |view, _, window, cx| {
                                view.open_image_preview(preview.clone(), window, cx);
                            }))
                    })
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child(if attachment.is_image { "IMG" } else { "FILE" }),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(PRIMARY))
                            .child(attachment.display_name.clone()),
                    )
                    .into_any_element()
            })
            .collect()
    }

    fn markdown_inline(
        nodes: &[MarkdownNode],
        style: &MarkdownInlineStyle,
        message_id: &str,
        element_index: &mut usize,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let mut elements = Vec::new();
        for node in nodes {
            match node {
                MarkdownNode::Container(tag, children) => {
                    let mut child_style = style.clone();
                    match tag {
                        MarkdownTag::Strong => child_style.marks |= MARKDOWN_STRONG,
                        MarkdownTag::Emphasis => child_style.marks |= MARKDOWN_EMPHASIS,
                        MarkdownTag::Strikethrough => {
                            child_style.marks |= MARKDOWN_STRIKETHROUGH;
                        }
                        MarkdownTag::Link(target) | MarkdownTag::Image(target) => {
                            child_style.link = safe_markdown_url(target);
                        }
                        _ => {}
                    }
                    elements.extend(Self::markdown_inline(
                        children,
                        &child_style,
                        message_id,
                        element_index,
                        cx,
                    ));
                }
                MarkdownNode::Text(text) | MarkdownNode::Code(text) | MarkdownNode::Html(text) => {
                    let is_code = matches!(node, MarkdownNode::Code(_));
                    let link = style.link.clone();
                    let element_id =
                        SharedString::from(format!("markdown-{message_id}-{}", *element_index));
                    *element_index += 1;
                    elements.push(
                        div()
                            .id(element_id)
                            .min_w_0()
                            .when(style.has(MARKDOWN_STRONG), |text| {
                                text.font_weight(gpui::FontWeight::BOLD)
                            })
                            .when(style.has(MARKDOWN_EMPHASIS), gpui::Styled::italic)
                            .when(
                                style.has(MARKDOWN_STRIKETHROUGH),
                                gpui::Styled::line_through,
                            )
                            .when(is_code, |text| {
                                text.px_1()
                                    .rounded_sm()
                                    .bg(rgb(SUBTLE))
                                    .font_family(".ZedMono")
                            })
                            .when(link.is_some(), |text| {
                                text.text_color(rgb(BLUE))
                                    .underline()
                                    .hover(gpui::Styled::cursor_pointer)
                            })
                            .when_some(link, |text, target| {
                                text.on_click(cx.listener(move |_, _, _, cx| {
                                    cx.open_url(&target);
                                }))
                            })
                            .child(text.clone())
                            .into_any_element(),
                    );
                }
                MarkdownNode::SoftBreak => {
                    elements.push(div().child(" ").into_any_element());
                }
                MarkdownNode::HardBreak => {
                    elements.push(div().w_full().h(px(0.)).into_any_element());
                }
                MarkdownNode::TaskMarker(checked) => {
                    elements.push(
                        div()
                            .font_family(".ZedMono")
                            .child(if *checked { "[x] " } else { "[ ] " })
                            .into_any_element(),
                    );
                }
                MarkdownNode::Rule => {}
            }
        }
        elements
    }

    fn markdown_inline_block(
        nodes: &[MarkdownNode],
        message_id: &str,
        element_index: &mut usize,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        div()
            .flex()
            .flex_wrap()
            .min_w_0()
            .children(Self::markdown_inline(
                nodes,
                &MarkdownInlineStyle::default(),
                message_id,
                element_index,
                cx,
            ))
            .into_any_element()
    }

    fn markdown_table_section(
        nodes: &[MarkdownNode],
        header: bool,
        message_id: &str,
        element_index: &mut usize,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        if header
            && nodes
                .iter()
                .all(|node| matches!(node, MarkdownNode::Container(MarkdownTag::TableCell, _)))
        {
            return vec![Self::markdown_table_row(
                nodes,
                true,
                message_id,
                element_index,
                cx,
            )];
        }

        nodes
            .iter()
            .filter_map(|node| {
                let MarkdownNode::Container(MarkdownTag::TableRow, cells) = node else {
                    return None;
                };
                Some(Self::markdown_table_row(
                    cells,
                    header,
                    message_id,
                    element_index,
                    cx,
                ))
            })
            .collect()
    }

    fn markdown_table_row(
        cells: &[MarkdownNode],
        header: bool,
        message_id: &str,
        element_index: &mut usize,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        div()
            .flex()
            .min_w_full()
            .children(cells.iter().filter_map(|cell| {
                let MarkdownNode::Container(MarkdownTag::TableCell, content) = cell else {
                    return None;
                };
                Some(
                    div()
                        .min_w(px(120.))
                        .flex_1()
                        .p_2()
                        .border_b_1()
                        .border_r_1()
                        .border_color(rgb(BORDER))
                        .when(header, |cell| {
                            cell.bg(rgb(SUBTLE)).font_weight(gpui::FontWeight::SEMIBOLD)
                        })
                        .child(Self::markdown_inline_block(
                            content,
                            message_id,
                            element_index,
                            cx,
                        )),
                )
            }))
            .into_any_element()
    }

    fn markdown_heading(
        level: u8,
        children: &[MarkdownNode],
        message_id: &str,
        element_index: &mut usize,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        div()
            .mt_1()
            .when(level <= 2, |heading| {
                heading.text_xl().font_weight(gpui::FontWeight::BOLD)
            })
            .when(level == 3, |heading| {
                heading.text_lg().font_weight(gpui::FontWeight::BOLD)
            })
            .when(level >= 4, |heading| {
                heading.font_weight(gpui::FontWeight::SEMIBOLD)
            })
            .child(Self::markdown_inline_block(
                children,
                message_id,
                element_index,
                cx,
            ))
            .into_any_element()
    }

    fn markdown_quote(
        children: &[MarkdownNode],
        message_id: &str,
        element_index: &mut usize,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        div()
            .pl_3()
            .border_l_2()
            .border_color(rgb(MUTED))
            .text_color(rgb(MUTED))
            .flex()
            .flex_col()
            .gap_2()
            .children(
                children
                    .iter()
                    .map(|child| Self::markdown_block(child, message_id, element_index, cx)),
            )
            .into_any_element()
    }

    fn markdown_code_block(
        language: Option<&String>,
        children: &[MarkdownNode],
        message_id: &str,
        element_index: &mut usize,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let code = markdown::plain_text(children);
        let copy = code.clone();
        let block_index = *element_index;
        *element_index += 1;
        div()
            .rounded_md()
            .border_1()
            .border_color(rgb(BORDER))
            .bg(rgb(SUBTLE))
            .overflow_hidden()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .child(language.cloned().unwrap_or_else(|| "code".to_owned()))
                    .child(
                        div()
                            .id(SharedString::from(format!(
                                "copy-code-{message_id}-{block_index}"
                            )))
                            .role(Role::Button)
                            .aria_label("Copy code")
                            .focusable()
                            .tab_stop(true)
                            .px_2()
                            .rounded_sm()
                            .child("Copy")
                            .hover(|style| style.bg(rgb(ELEVATED)).cursor_pointer())
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                    copy.clone(),
                                ));
                            })),
                    ),
            )
            .child(
                div()
                    .id(SharedString::from(format!(
                        "code-content-{message_id}-{block_index}"
                    )))
                    .debug_selector(|| "markdown-code".to_owned())
                    .p_3()
                    .overflow_x_scroll()
                    .whitespace_nowrap()
                    .font_family(".ZedMono")
                    .text_sm()
                    .child(code),
            )
            .into_any_element()
    }

    fn markdown_list(
        start: Option<u64>,
        children: &[MarkdownNode],
        message_id: &str,
        element_index: &mut usize,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let first = start.unwrap_or(1);
        div()
            .flex()
            .flex_col()
            .gap_1()
            .children(children.iter().enumerate().map(|(index, child)| {
                let marker = if start.is_some() {
                    format!("{}.", first + u64::try_from(index).unwrap_or(0))
                } else {
                    "•".to_owned()
                };
                let content = match child {
                    MarkdownNode::Container(MarkdownTag::Item, content) => content,
                    _ => std::slice::from_ref(child),
                };
                div()
                    .flex()
                    .items_start()
                    .gap_2()
                    .child(
                        div()
                            .w(px(24.))
                            .flex_shrink_0()
                            .text_color(rgb(MUTED))
                            .child(marker),
                    )
                    .child(
                        div().min_w_0().flex_1().flex().flex_col().gap_1().children(
                            content.iter().map(|item| {
                                Self::markdown_block(item, message_id, element_index, cx)
                            }),
                        ),
                    )
            }))
            .into_any_element()
    }

    fn markdown_table(
        children: &[MarkdownNode],
        message_id: &str,
        element_index: &mut usize,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let table_index = *element_index;
        *element_index += 1;
        let mut rows = Vec::new();
        for child in children {
            match child {
                MarkdownNode::Container(MarkdownTag::TableHead, head) => {
                    rows.extend(Self::markdown_table_section(
                        head,
                        true,
                        message_id,
                        element_index,
                        cx,
                    ));
                }
                MarkdownNode::Container(MarkdownTag::TableRow, _) => {
                    rows.extend(Self::markdown_table_section(
                        std::slice::from_ref(child),
                        false,
                        message_id,
                        element_index,
                        cx,
                    ));
                }
                _ => {}
            }
        }
        div()
            .id(SharedString::from(format!(
                "markdown-table-{message_id}-{table_index}"
            )))
            .debug_selector(|| "markdown-table".to_owned())
            .overflow_x_scroll()
            .rounded_md()
            .border_1()
            .border_color(rgb(BORDER))
            .children(rows)
            .into_any_element()
    }

    fn markdown_block(
        node: &MarkdownNode,
        message_id: &str,
        element_index: &mut usize,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        match node {
            MarkdownNode::Container(MarkdownTag::Paragraph, children) => {
                Self::markdown_inline_block(children, message_id, element_index, cx)
            }
            MarkdownNode::Container(MarkdownTag::Heading(level), children) => {
                Self::markdown_heading(*level, children, message_id, element_index, cx)
            }
            MarkdownNode::Container(MarkdownTag::BlockQuote, children) => {
                Self::markdown_quote(children, message_id, element_index, cx)
            }
            MarkdownNode::Container(MarkdownTag::CodeBlock(language), children) => {
                Self::markdown_code_block(
                    language.as_ref(),
                    children,
                    message_id,
                    element_index,
                    cx,
                )
            }
            MarkdownNode::Container(MarkdownTag::List(start), children) => {
                Self::markdown_list(*start, children, message_id, element_index, cx)
            }
            MarkdownNode::Container(MarkdownTag::Table, children) => {
                Self::markdown_table(children, message_id, element_index, cx)
            }
            MarkdownNode::Rule => div()
                .w_full()
                .h(px(1.))
                .my_2()
                .bg(rgb(BORDER))
                .into_any_element(),
            MarkdownNode::Container(_, children) => div()
                .flex()
                .flex_col()
                .gap_1()
                .children(
                    children
                        .iter()
                        .map(|child| Self::markdown_block(child, message_id, element_index, cx)),
                )
                .into_any_element(),
            _ => Self::markdown_inline_block(
                std::slice::from_ref(node),
                message_id,
                element_index,
                cx,
            ),
        }
    }

    fn markdown_content(
        message_id: &str,
        source: &str,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let document = markdown::parse(source);
        let mut element_index = 0;
        div()
            .id(SharedString::from(format!("markdown-content-{message_id}")))
            .debug_selector(|| "markdown-content".to_owned())
            .flex()
            .flex_col()
            .gap_2()
            .children(
                document
                    .children
                    .iter()
                    .map(|node| Self::markdown_block(node, message_id, &mut element_index, cx)),
            )
            .into_any_element()
    }

    fn transcript_message(
        message: &app_model::TranscriptMessage,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_user = message.role == TranscriptRole::User;
        let speaker = if is_user { "You" } else { "Copilot" };
        let attachments = Self::message_attachment_chips(message, cx);
        let markdown_source = message.content.clone();
        let markdown = Self::markdown_content(&message.id, &message.content, cx);
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
                    .debug_selector(|| "transcript-message".to_owned())
                    .w_full()
                    .min_w_0()
                    .p_3()
                    .rounded_lg()
                    .bg(if is_user { rgb(ELEVATED) } else { rgb(PANEL) })
                    .border_1()
                    .border_color(rgb(BORDER))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(if is_user { rgb(BLUE) } else { rgb(GREEN) })
                                    .child(if is_user { "You" } else { "Copilot" }),
                            )
                            .when(!message.content.is_empty(), |header| {
                                header.child(
                                    div()
                                        .id(SharedString::from(format!(
                                            "copy-markdown-{}",
                                            message.id
                                        )))
                                        .debug_selector(|| "copy-markdown".to_owned())
                                        .role(Role::Button)
                                        .aria_label("Copy original markdown")
                                        .focusable()
                                        .tab_stop(true)
                                        .px_1()
                                        .rounded_sm()
                                        .text_xs()
                                        .text_color(rgb(MUTED))
                                        .child("Copy")
                                        .hover(|style| {
                                            style
                                                .bg(rgb(SUBTLE))
                                                .text_color(rgb(PRIMARY))
                                                .cursor_pointer()
                                        })
                                        .on_click(cx.listener(move |_, _, _, cx| {
                                            cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                                markdown_source.clone(),
                                            ));
                                        })),
                                )
                            }),
                    )
                    .when(!message.content.is_empty(), |bubble| {
                        bubble.child(div().mt_2().text_color(rgb(PRIMARY)).child(markdown))
                    })
                    // Shown after the text, mirroring the composer, so the
                    // message reads back the way it was written.
                    .when(!attachments.is_empty(), |bubble| {
                        bubble.child(
                            div()
                                .id(SharedString::from(format!(
                                    "message-attachments-{}",
                                    message.id
                                )))
                                .debug_selector(|| "message-attachments".to_owned())
                                .mt_2()
                                .flex()
                                .flex_wrap()
                                .gap_2()
                                .children(attachments),
                        )
                    })
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

    fn transcript(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
                    Self::transcript_message(message, cx).into_any_element()
                }
                app_model::TimelineEntry::Tool(invocation) => {
                    let children = session
                        .snapshot
                        .tool_activity
                        .children_of(&invocation.call_id);
                    self.tool_entry(invocation, &children, cx)
                        .into_any_element()
                }
            })
            .collect::<Vec<_>>();
        let group = SharedString::from("scroll-transcript");
        let scrollbar = Self::scrollbar(
            TRANSCRIPT_SCROLL_ID,
            &self.transcript_scroll,
            group.clone(),
            cx,
        );

        div()
            .id("transcript-frame")
            .group(group)
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(
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
                    .overflow_y_scroll()
                    .child(
                        div()
                            .debug_selector(|| "transcript-content".to_owned())
                            .mx_auto()
                            .w_full()
                            .max_w(px(CONVERSATION_COLUMN_WIDTH))
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .children(entries),
                    ),
            )
            .children(scrollbar)
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
            let (state_label, state_color) = terminal_state_display(terminal.state);
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
                    "shell {} · {} call(s) · {} bytes in {} chunk(s)",
                    terminal.shell_id,
                    terminal.tool_call_ids.len(),
                    terminal.output_metadata.byte_count,
                    terminal.output_metadata.chunk_count
                )))
                .child(
                    div()
                        .max_h(px(160.0))
                        .overflow_hidden()
                        .text_xs()
                        .text_color(rgb(PRIMARY))
                        .child(terminal_tail(&terminal.output)),
                )
                .when_some(terminal_output_error(terminal), |card, error| {
                    card.child(
                        div()
                            .id(SharedString::from(format!(
                                "terminal-output-error-{}",
                                terminal.shell_id
                            )))
                            .role(Role::Alert)
                            .text_xs()
                            .text_color(rgb(RED))
                            .child(format!("Output unavailable: {error}")),
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
        let has_draft =
            !self.composer.read(cx).value().trim().is_empty() || !self.draft_attachments.is_empty();
        let stops_running_session = running && !has_draft;
        let action_id = if stops_running_session {
            "stop-session"
        } else {
            "submit-prompt"
        };
        let action_label = if stops_running_session {
            "Stop agent"
        } else if running {
            "Send steering message"
        } else {
            "Send message"
        };
        let disconnected =
            selected.is_some_and(|session| session.snapshot.status == SessionStatus::Disconnected);
        let resume = disconnected
            .then(|| self.selected_session.clone())
            .flatten();
        div()
            .id("composer")
            .debug_selector(|| "composer".to_owned())
            .accessibility_id("composer")
            .relative()
            .role(Role::Group)
            .aria_label("Message composer")
            .on_drop(cx.listener(|view, paths: &ExternalPaths, _, cx| {
                view.attach_dropped_paths(paths.paths(), cx);
            }))
            .drag_over::<ExternalPaths>(|style, _, _, _| style.border_color(rgb(BLUE)))
            .mx_auto()
            .mb_4()
            .w_full()
            .max_w(px(CONVERSATION_COLUMN_WIDTH))
            .flex()
            .flex_col()
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(BORDER))
            .rounded_lg()
            .shadow_lg()
            .child(self.composer.clone())
            .children(self.attachment_strip(cx))
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
                            .accessibility_id("attachments-placeholder")
                            .role(Role::Button)
                            .aria_label("Attach files")
                            .focusable()
                            .tab_stop(true)
                            .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                            .text_lg()
                            .text_color(rgb(MUTED))
                            .child("+")
                            .hover(|style| style.text_color(rgb(PRIMARY)).cursor_pointer())
                            .on_click(cx.listener(|_, _, _, cx| Self::pick_attachments(cx))),
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
                            .id(action_id)
                            .debug_selector(move || action_id.to_owned())
                            .accessibility_id(action_id)
                            .role(Role::Button)
                            .aria_label(action_label)
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
                            .text_color(if stops_running_session {
                                rgb(RED)
                            } else {
                                rgb(MUTED)
                            })
                            .child(if stops_running_session { "■" } else { "↑" })
                            .hover(|style| {
                                style
                                    .bg(rgb(BORDER))
                                    .text_color(rgb(PRIMARY))
                                    .cursor_pointer()
                            })
                            .on_click(cx.listener(move |view, _, _, cx| {
                                if stops_running_session {
                                    if let Some(app_session_id) = view.selected_session.clone() {
                                        let _ = view
                                            .commands
                                            .send(ServiceCommand::Cancel { app_session_id });
                                    }
                                } else {
                                    view.submit_composer(cx);
                                }
                            })),
                    )
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
            .on_drop(cx.listener(|view, paths: &ExternalPaths, _, cx| {
                view.attach_dropped_paths(paths.paths(), cx);
            }))
            .drag_over::<ExternalPaths>(|style, _, _, _| style.border_color(rgb(BLUE)))
            .relative()
            .w_full()
            .max_w(px(CONVERSATION_COLUMN_WIDTH))
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
                    .children(self.attachment_strip(cx))
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
                                    .accessibility_id("home-attachments-placeholder")
                                    .role(Role::Button)
                                    .aria_label("Attach files")
                                    .focusable()
                                    .tab_stop(true)
                                    .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
                                    .w(px(28.0))
                                    .h(px(28.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded_md()
                                    .text_lg()
                                    .text_color(rgb(MUTED))
                                    .child("+")
                                    .hover(|style| style.text_color(rgb(PRIMARY)).cursor_pointer())
                                    .on_click(
                                        cx.listener(|_, _, _, cx| Self::pick_attachments(cx)),
                                    ),
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
        let app_session_id = session.id().to_owned();
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
        self.note_transcript_extent(cx);
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
                view.dismiss_image_preview(cx);
                view.settings_visibility = SettingsVisibility::Closed;
                if view.renaming_session.is_some() {
                    view.cancel_rename(cx);
                }
            }))
            .relative()
            .flex()
            .size_full()
            .bg(rgb(BACKGROUND))
            .text_color(rgb(PRIMARY))
            // Scrollbar drags are tracked at the window so the thumb keeps
            // following the pointer once it leaves the narrow track.
            .on_mouse_move(cx.listener(|view, event: &gpui::MouseMoveEvent, _, cx| {
                if let Some(drag) = view.dragging_scrollbar.clone() {
                    if event.pressed_button == Some(MouseButton::Left) {
                        view.drag_scrollbar_to(&drag.id, event.position.y, drag.grab_offset);
                        cx.notify();
                    } else {
                        view.dragging_scrollbar = None;
                    }
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|view, _, _, _| {
                    view.dragging_scrollbar = None;
                }),
            )
            .when(show_sidebar, |root| root.child(self.sidebar(compact, cx)))
            .child(
                div()
                    .id("main-content")
                    .relative()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .when_some(self.update_banner(cx), gpui::ParentElement::child)
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
                                .child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .flex_1()
                                        .min_w_0()
                                        .min_h_0()
                                        .child(self.transcript(cx))
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
                                        .child(
                                            div().w_full().px_5().child(self.session_composer(cx)),
                                        ),
                                )
                                .when_some(
                                    if self.panel_open {
                                        self.side_panel(cx)
                                    } else {
                                        None
                                    },
                                    gpui::ParentElement::child,
                                ),
                        )
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
                                .max_w(px(CONVERSATION_COLUMN_WIDTH))
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
            .when_some(self.settings_dialog(cx), gpui::ParentElement::child)
            .when_some(self.image_preview_overlay(cx), gpui::ParentElement::child)
            .when_some(self.interaction_dialog(cx), |root, dialog| {
                root.child(dialog)
            })
    }
}

/// Trailing slice of terminal output displayed until transcript virtualization.
fn terminal_tail(output: &str) -> String {
    tail_lines(output, 40)
}

fn terminal_state_display(state: app_model::TerminalState) -> (&'static str, u32) {
    match state {
        app_model::TerminalState::Running => ("running", GREEN),
        app_model::TerminalState::Exited => ("exited", MUTED),
        app_model::TerminalState::Cancelled => ("cancelled", RED),
    }
}

fn terminal_output_error(terminal: &app_model::TerminalSession) -> Option<String> {
    terminal
        .output_load_error
        .clone()
        .or_else(|| terminal.output_error.clone())
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
            let blocking = session
                .snapshot
                .capabilities
                .blocking_for(session.snapshot.metadata.kind)
                .len();
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
        .debug_selector(move || label.to_owned())
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

fn data_directory() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("GCABB_DATA_DIR") {
        return Ok(PathBuf::from(path));
    }
    dirs::data_local_dir()
        .map(|base| base.join(DATA_DIRECTORY_NAME))
        .ok_or_else(|| "operating system did not provide a local data directory".to_owned())
}

/// Moves data written by older builds out of the replaceable install directory.
///
/// During an update, the old updater moves the complete installation to its
/// backup before launching the new build. Migrating before that backup is
/// cleaned preserves the database and user-created files across the transition
/// to the dedicated data directory.
fn prepare_data_directory_for_build(build: &BuildStamp) -> Result<PathBuf, String> {
    let data_dir = data_directory()?;
    if build.is_release() {
        let layout = InstallLayout::for_running_executable().map_err(|error| error.to_string())?;
        if std::env::var_os("GCABB_DATA_DIR").is_none() {
            let legacy = dirs::data_local_dir().map(|base| base.join("gcabb"));
            let mut sources = vec![layout.backup_root.clone()];
            if let Some(legacy) = legacy {
                sources.push(legacy);
            }
            migrate_persistent_data(&data_dir, &sources)?;
        }
        // Data is now independent of the installation, so the rollback copy can
        // be removed without deleting session state.
        layout.clean_completed_updates();
    }
    Ok(data_dir)
}

fn database_path(data_dir: &Path) -> Result<PathBuf, String> {
    prepare_data_directory(data_dir)
}

fn migrate_persistent_data(target: &Path, sources: &[PathBuf]) -> Result<(), String> {
    if target.exists() {
        return Ok(());
    }
    let source_with_entries = || {
        sources.iter().find(|source| {
            PERSISTENT_DATA_ENTRIES
                .iter()
                .any(|entry| source.join(entry).exists())
        })
    };
    let Some(source) = sources
        .iter()
        .find(|source| source.join("gcabb.db").exists())
        .or_else(source_with_entries)
    else {
        return Ok(());
    };

    let staging = target.with_extension("migrating");
    if staging.exists() {
        std::fs::remove_dir_all(&staging)
            .map_err(|error| format!("failed to clear {}: {error}", staging.display()))?;
    }
    std::fs::create_dir_all(&staging)
        .map_err(|error| format!("failed to create {}: {error}", staging.display()))?;
    for entry in PERSISTENT_DATA_ENTRIES {
        let from = source.join(entry);
        if from.exists() {
            copy_persistent_path(&from, &staging.join(entry))?;
        }
    }
    std::fs::rename(&staging, target).map_err(|error| {
        format!(
            "failed to finish data migration from {} to {}: {error}",
            source.display(),
            target.display()
        )
    })
}

fn copy_persistent_path(from: &Path, to: &Path) -> Result<(), String> {
    if from.is_dir() {
        std::fs::create_dir_all(to)
            .map_err(|error| format!("failed to create {}: {error}", to.display()))?;
        let entries = std::fs::read_dir(from)
            .map_err(|error| format!("failed to read {}: {error}", from.display()))?;
        for entry in entries {
            let entry =
                entry.map_err(|error| format!("failed to read {}: {error}", from.display()))?;
            copy_persistent_path(&entry.path(), &to.join(entry.file_name()))?;
        }
    } else {
        std::fs::copy(from, to).map_err(|error| {
            format!(
                "failed to copy {} to {}: {error}",
                from.display(),
                to.display()
            )
        })?;
    }
    Ok(())
}

/// Working directory for chats.
///
/// Chats have no repository, but the CLI still needs a valid working
/// directory. A dedicated folder under the app data directory keeps chat tool
/// activity away from any checkout; if it cannot be created, fall back to the
/// launch directory so chats still work.
fn chats_directory(fallback: &Path) -> PathBuf {
    let Ok(base) = data_directory() else {
        return fallback.to_owned();
    };
    let path = base.join("chats");
    if std::fs::create_dir_all(&path).is_err() {
        return fallback.to_owned();
    }
    path
}

/// Where pasted images are kept.
///
/// Deliberately not the session worktree: files written there would appear in
/// the changes view and could be committed by accident. The runtime references
/// an attached file in place rather than copying it, so this has to outlive the
/// composer for the transcript to still show the picture later.
fn attachments_directory() -> Option<PathBuf> {
    let base = data_directory().ok()?;
    let path = base.join("attachments");
    std::fs::create_dir_all(&path).ok()?;
    Some(path)
}

/// Write a pasted image to disk so it can be referenced by path.
fn write_pasted_image(
    directory: &Path,
    bytes: &[u8],
    mime_type: &str,
    index: usize,
) -> Option<PromptAttachment> {
    let extension = match mime_type {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "image/bmp" => "bmp",
        _ => return None,
    };
    let path = directory.join(format!("{}-clipboard.{extension}", uuid::Uuid::new_v4()));
    std::fs::write(&path, bytes).ok()?;
    Some(PromptAttachment::File {
        path: path.to_string_lossy().into_owned(),
        display_name: format!("Pasted image {index}"),
    })
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
    let root = root.canonicalize().unwrap_or_else(|_| root.to_owned());
    git_output(&root, &["worktree", "list", "--porcelain"])
        .and_then(|output| {
            output
                .lines()
                .find_map(|line| line.strip_prefix("worktree ").map(str::to_owned))
        })
        .map_or(root, |path| {
            let path = PathBuf::from(path);
            path.canonicalize().unwrap_or(path)
        })
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

fn update_poll_delay() -> Duration {
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
        ^ u64::from(std::process::id());
    update_poll_delay_for(seed)
}

fn update_poll_delay_for(seed: u64) -> Duration {
    let jitter_seconds = UPDATE_POLL_JITTER.as_secs();
    let offset = seed % (jitter_seconds * 2 + 1);
    UPDATE_POLL_INTERVAL.saturating_sub(UPDATE_POLL_JITTER) + Duration::from_secs(offset)
}

fn timestamp() -> String {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or_else(
        |_| "0".to_owned(),
        |duration| duration.as_millis().to_string(),
    )
}

fn elapsed_millis(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Install every key binding the app responds to.
///
/// Shared with the interaction tests so they exercise the bindings the app
/// actually ships. Tests that installed their own bindings once let a
/// macOS-only paste shortcut reach Linux users unnoticed.
fn bind_app_keys(cx: &mut App) {
    bind_text_input_keys(cx);
    cx.bind_keys([
        KeyBinding::new("escape", DismissPopup, None),
        KeyBinding::new("tab", FocusNext, None),
        KeyBinding::new("shift-tab", FocusPrevious, None),
    ]);
}

/// Records the running build's identity.
fn resolve_build_identity() -> BuildStamp {
    let build = BuildStamp::current();
    tracing::info!(
        version = %build.version,
        channel = %build.channel,
        commit = build.commit.as_deref().unwrap_or("unknown"),
        target = build.target,
        release = build.is_release(),
        "gcabb build identity"
    );
    build
}

/// How the binary was asked to run.
enum Invocation {
    /// Open the application window.
    Desktop,
    /// Print the build identity and exit.
    Version,
    /// Report whether an update is available and exit.
    CheckUpdate,
    /// Apply an available update and exit.
    ApplyUpdate,
    Help,
    Unknown(String),
}

fn invocation() -> Invocation {
    match std::env::args().nth(1).as_deref() {
        None => Invocation::Desktop,
        Some("--version" | "-V") => Invocation::Version,
        Some("--check-update") => Invocation::CheckUpdate,
        Some("--apply-update") => Invocation::ApplyUpdate,
        Some("--help" | "-h") => Invocation::Help,
        Some(other) => Invocation::Unknown(other.to_owned()),
    }
}

const USAGE: &str = "\
GCABB

Usage:
  gcabb-desktop                 Open the application
  gcabb-desktop --version       Print the build identity
  gcabb-desktop --check-update  Report whether an update is available
  gcabb-desktop --apply-update  Download, verify, and apply an available update
  gcabb-desktop --help          Show this message

Exit codes for the update commands:
  0  an update is available, or was applied
  1  the check or the update failed
  2  nothing to do
";

fn main() {
    if let Err(error) = init_tracing("gcabb=info") {
        eprintln!("failed to initialize structured tracing: {error}");
    }
    if let Some(code) = updates::run_update_helper_if_requested() {
        std::process::exit(code);
    }
    let build = resolve_build_identity();
    let data_dir = prepare_data_directory_for_build(&build);

    // The update commands run the same code the window drives, so CI can
    // exercise the loop on each platform without driving a GUI.
    match invocation() {
        Invocation::Desktop => {}
        Invocation::Version => {
            println!("{}", build.display());
            return;
        }
        Invocation::Help => {
            print!("{USAGE}");
            return;
        }
        Invocation::Unknown(argument) => {
            eprintln!("unrecognised argument {argument}\n");
            print!("{USAGE}");
            std::process::exit(1);
        }
        command @ (Invocation::CheckUpdate | Invocation::ApplyUpdate) => {
            let apply = matches!(command, Invocation::ApplyUpdate);
            let code = match &data_dir {
                Ok(data_dir) => updates::run_headless(&build, data_dir, apply),
                Err(error) => {
                    eprintln!("{error}");
                    1
                }
            };
            std::process::exit(code);
        }
    }

    let window_title = format!("GCABB {}", build.display());
    let project_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let branch = git_branch(&project_root);
    let service = match data_dir.and_then(|path| database_path(&path)) {
        Ok(path) => AppService::start(project_root.clone(), &path),
        Err(error) => AppService::failed(error),
    };
    let chats_workspace = chats_directory(&project_root);

    gpui_platform::application().run(move |cx: &mut App| {
        bind_app_keys(cx);
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
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
                        title: Some(window_title.clone().into()),
                        ..Default::default()
                    }),
                    app_id: Some(APP_ID.to_owned()),
                    window_min_size: Some(size(px(640.0), px(520.0))),
                    ..Default::default()
                },
                move |_, cx| {
                    cx.new(|cx| {
                        SessionMvpView::new(
                            service,
                            project_root,
                            branch,
                            chats_workspace,
                            attachments_directory(),
                            cx,
                        )
                    })
                },
            )
            .expect("failed to open GCABB window");
        window
            .update(cx, |view, window, cx| {
                window.activate_window();
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
        COMPACT_WIDTH, ControlMenu, UPDATE_POLL_INTERVAL, UPDATE_POLL_JITTER, compact_layout,
        context_window_label, control_menu_id, control_menu_offset, default_branch,
        default_context_tier, effort_label, migrate_persistent_data, reasoning_effort_for_model,
        repository_root, toggled_menu, token_label, update_poll_delay_for,
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

    #[test]
    fn update_poll_jitter_stays_within_the_six_hour_window() {
        let minimum = UPDATE_POLL_INTERVAL.saturating_sub(UPDATE_POLL_JITTER);
        let maximum = UPDATE_POLL_INTERVAL + UPDATE_POLL_JITTER;

        assert_eq!(update_poll_delay_for(0), minimum);
        assert!(update_poll_delay_for(u64::MAX) <= maximum);
    }

    /// Adding a worktree folder must resolve to its repository, so adding a
    /// worktree and its main checkout cannot create two projects.
    #[test]
    fn adding_a_worktree_folder_resolves_to_the_repository() {
        let (_guard, main, worktree) = repo_with_worktree();
        let canonical_main = main.canonicalize().expect("canonical main worktree");
        assert_eq!(repository_root(&worktree), canonical_main);
        assert_eq!(repository_root(&main), canonical_main);
    }

    /// A plain directory that is not a repository is still usable as a project.
    #[test]
    fn adding_a_non_repository_folder_keeps_the_folder() {
        let dir = tempfile::tempdir().expect("tempdir");
        let canonical = dir.path().canonicalize().expect("canonical tempdir");
        assert_eq!(repository_root(dir.path()), canonical);
        assert!(default_branch(dir.path()).is_none());
    }

    #[test]
    fn update_backup_data_is_migrated_without_installation_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let backup = dir.path().join(".GCABB-update-backup");
        let target = dir.path().join("GCABB-data");
        std::fs::create_dir_all(backup.join("attachments")).expect("attachments");
        std::fs::write(backup.join("gcabb.db"), b"database").expect("database");
        std::fs::write(backup.join("gcabb.db-wal"), b"wal").expect("wal");
        std::fs::write(backup.join("attachments").join("image.png"), b"image").expect("attachment");
        std::fs::write(backup.join("gcabb-desktop.exe"), b"binary").expect("binary");

        migrate_persistent_data(&target, &[backup]).expect("migration");

        assert_eq!(
            std::fs::read(target.join("gcabb.db")).expect("migrated database"),
            b"database"
        );
        assert_eq!(
            std::fs::read(target.join("gcabb.db-wal")).expect("migrated wal"),
            b"wal"
        );
        assert_eq!(
            std::fs::read(target.join("attachments").join("image.png"))
                .expect("migrated attachment"),
            b"image"
        );
        assert!(!target.join("gcabb-desktop.exe").exists());
    }

    #[test]
    fn existing_data_directory_is_never_overwritten_by_a_backup() {
        let dir = tempfile::tempdir().expect("tempdir");
        let backup = dir.path().join(".GCABB-update-backup");
        let target = dir.path().join("GCABB-data");
        std::fs::create_dir_all(&backup).expect("backup");
        std::fs::create_dir_all(&target).expect("target");
        std::fs::write(backup.join("gcabb.db"), b"old").expect("old database");
        std::fs::write(target.join("gcabb.db"), b"current").expect("current database");

        migrate_persistent_data(&target, &[backup]).expect("migration");

        assert_eq!(
            std::fs::read(target.join("gcabb.db")).expect("current database"),
            b"current"
        );
    }

    #[test]
    fn migration_prefers_the_source_containing_the_database() {
        let dir = tempfile::tempdir().expect("tempdir");
        let incomplete_backup = dir.path().join(".GCABB-update-backup");
        let legacy = dir.path().join("GCABB");
        let target = dir.path().join("GCABB-data");
        std::fs::create_dir_all(&incomplete_backup).expect("backup");
        std::fs::create_dir_all(&legacy).expect("legacy");
        std::fs::write(
            incomplete_backup.join("update-settings.json"),
            b"incomplete",
        )
        .expect("settings");
        std::fs::write(legacy.join("gcabb.db"), b"database").expect("database");

        migrate_persistent_data(&target, &[incomplete_backup, legacy]).expect("migration");

        assert_eq!(
            std::fs::read(target.join("gcabb.db")).expect("migrated database"),
            b"database"
        );
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
        use app_model::{
            SessionKind, SessionMetadata, SessionSnapshot, SessionStatus, TitleSource,
        };
        use gpui::{Modifiers, MouseButton, TestAppContext, VisualTestContext};
        use session_manager::SessionHandle;
        use std::sync::Arc;

        use crate::{
            AppService, ServiceCommand, ServiceUpdate, SessionMvpView, SessionProjection, UpdateUi,
        };

        fn snapshot(id: &str, title: &str) -> SessionSnapshot {
            let mut state = SessionSnapshot::new(SessionMetadata {
                id: id.to_owned(),
                sdk_session_id: format!("sdk-{id}"),
                project_path: "/tmp/project".to_owned(),
                repository_root: Some("/tmp/project".to_owned()),
                title: title.to_owned(),
                title_source: TitleSource::Manual,
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
            let (view, cx, commands, _) = setup_with_attachments(cx);
            (view, cx, commands)
        }

        fn setup_for_bootstrap(
            cx: &mut TestAppContext,
        ) -> (
            gpui::Entity<SessionMvpView>,
            &mut VisualTestContext,
            std::sync::mpsc::Receiver<ServiceCommand>,
            std::sync::mpsc::Sender<ServiceUpdate>,
        ) {
            let (service, commands, updates) = AppService::for_test_with_updates();
            cx.update(super::super::bind_app_keys);
            let (view, cx) = cx.add_window_view(|_, cx| {
                SessionMvpView::new(
                    service,
                    std::path::PathBuf::from("/tmp/project"),
                    "main".to_owned(),
                    std::path::PathBuf::from("/tmp/chats"),
                    None,
                    cx,
                )
            });
            cx.run_until_parked();
            (view, cx, commands, updates)
        }

        /// Same view, plus a temporary directory for pasted images.
        fn setup_with_attachments(
            cx: &mut TestAppContext,
        ) -> (
            gpui::Entity<SessionMvpView>,
            &mut VisualTestContext,
            std::sync::mpsc::Receiver<ServiceCommand>,
            tempfile::TempDir,
        ) {
            let attachments = tempfile::tempdir().expect("temp dir");
            let attachments_root = attachments.path().to_owned();
            let (service, commands) = AppService::for_test();
            cx.update(super::super::bind_app_keys);
            let (view, cx) = cx.add_window_view(|_, cx| {
                let mut view = SessionMvpView::new(
                    service,
                    std::path::PathBuf::from("/tmp/project"),
                    "main".to_owned(),
                    std::path::PathBuf::from("/tmp/chats"),
                    Some(attachments_root),
                    cx,
                );
                view.selected_project = std::path::PathBuf::from("/tmp/project");
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(
                    snapshot("session-1", "First session"),
                ))];
                view
            });
            cx.run_until_parked();
            (view, cx, commands, attachments)
        }

        fn assert_horizontally_aligned(
            label: &str,
            actual: gpui::Bounds<gpui::Pixels>,
            expected: gpui::Bounds<gpui::Pixels>,
        ) {
            let left_delta = f32::from(actual.origin.x - expected.origin.x).abs();
            let width_delta = f32::from(actual.size.width - expected.size.width).abs();
            assert!(
                left_delta < 0.5 && width_delta < 0.5,
                "{label} is not aligned: {actual:?} vs {expected:?}"
            );
        }

        #[gpui::test]
        fn bootstrap_selects_the_stored_session_before_hydration(cx: &mut TestAppContext) {
            let (mut service, _commands) = AppService::for_test();
            let first = snapshot("session-1", "First session").metadata;
            let second = snapshot("session-2", "Second session").metadata;
            service.bootstrap = Some(super::super::BootstrapState {
                projects: Vec::new(),
                sessions: vec![first, second],
                selected_session: Some("session-2".to_owned()),
            });
            cx.update(super::super::bind_app_keys);
            let (view, cx) = cx.add_window_view(|_, cx| {
                SessionMvpView::new(
                    service,
                    std::path::PathBuf::from("/tmp/project"),
                    "main".to_owned(),
                    std::path::PathBuf::from("/tmp/chats"),
                    None,
                    cx,
                )
            });

            view.read_with(cx, |view, _| {
                assert_eq!(view.selected_session.as_deref(), Some("session-2"));
                assert_eq!(view.sessions.len(), 2);
                assert_eq!(
                    view.selected().unwrap().snapshot.status,
                    SessionStatus::Recovering
                );
            });
        }

        #[gpui::test]
        fn navigation_before_bootstrap_is_never_overwritten(cx: &mut TestAppContext) {
            let (view, cx, _commands, _updates) = setup_for_bootstrap(cx);
            view.update(cx, SessionMvpView::new_session);
            view.update(cx, |view, _| {
                view.apply_bootstrap(super::super::BootstrapState {
                    projects: Vec::new(),
                    sessions: vec![snapshot("session-1", "First session").metadata],
                    selected_session: Some("session-1".to_owned()),
                });
            });

            view.read_with(cx, |view, _| {
                assert!(view.selected_session.is_none());
                assert_eq!(view.sessions.len(), 1);
            });
        }

        #[gpui::test]
        fn hydration_replaces_the_shell_without_changing_navigation(cx: &mut TestAppContext) {
            let (view, cx, _commands, updates) = setup_for_bootstrap(cx);
            view.update(cx, |view, _| {
                view.apply_bootstrap(super::super::BootstrapState {
                    projects: Vec::new(),
                    sessions: vec![snapshot("session-1", "First session").metadata],
                    selected_session: Some("session-1".to_owned()),
                });
            });
            view.update(cx, SessionMvpView::new_session);
            updates
                .send(ServiceUpdate::SessionHydrated(SessionHandle::for_test(
                    snapshot("session-1", "Hydrated session"),
                )))
                .unwrap();

            view.update(cx, SessionMvpView::apply_service_updates);

            view.read_with(cx, |view, _| {
                assert!(view.selected_session.is_none());
                assert_eq!(view.sessions[0].snapshot.status, SessionStatus::Idle);
                assert_eq!(view.sessions[0].snapshot.metadata.title, "Hydrated session");
            });
        }

        #[gpui::test]
        fn first_hydration_is_selected_when_bootstrap_metadata_was_empty(cx: &mut TestAppContext) {
            let (view, cx, _commands, updates) = setup_for_bootstrap(cx);
            updates
                .send(ServiceUpdate::SessionHydrated(SessionHandle::for_test(
                    snapshot("session-1", "Recovered session"),
                )))
                .unwrap();

            view.update(cx, SessionMvpView::apply_service_updates);

            view.read_with(cx, |view, _| {
                assert_eq!(view.selected_session.as_deref(), Some("session-1"));
            });
        }

        #[gpui::test]
        fn hydration_refreshes_repository_grouping_after_metadata_adoption(
            cx: &mut TestAppContext,
        ) {
            let (view, cx, _commands, updates) = setup_for_bootstrap(cx);
            let mut legacy = snapshot("session-1", "Legacy session").metadata;
            legacy.repository_root = None;
            legacy.project_path = "/tmp/repository/worktree".to_owned();
            view.update(cx, |view, _| {
                view.apply_bootstrap(super::super::BootstrapState {
                    projects: Vec::new(),
                    sessions: vec![legacy],
                    selected_session: Some("session-1".to_owned()),
                });
            });
            let mut adopted = snapshot("session-1", "Legacy session");
            adopted.metadata.project_path = "/tmp/repository/worktree".to_owned();
            adopted.metadata.repository_root = Some("/tmp/repository".to_owned());
            updates
                .send(ServiceUpdate::SessionHydrated(SessionHandle::for_test(
                    adopted,
                )))
                .unwrap();

            view.update(cx, SessionMvpView::apply_service_updates);

            view.read_with(cx, |view, _| {
                assert_eq!(
                    view.selected_project,
                    std::path::PathBuf::from("/tmp/repository")
                );
                assert_eq!(
                    view.workspace_root,
                    std::path::PathBuf::from("/tmp/repository/worktree")
                );
            });
        }

        #[gpui::test]
        fn restoration_failure_keeps_the_selected_shell_and_surfaces_error(
            cx: &mut TestAppContext,
        ) {
            let (view, cx, _commands, updates) = setup_for_bootstrap(cx);
            view.update(cx, |view, _| {
                view.apply_bootstrap(super::super::BootstrapState {
                    projects: Vec::new(),
                    sessions: vec![snapshot("session-1", "First session").metadata],
                    selected_session: Some("session-1".to_owned()),
                });
            });
            updates
                .send(ServiceUpdate::Ready {
                    compatibility: copilot_provider::ProviderCompatibility {
                        sdk_crate_version: "test".to_owned(),
                        sdk_protocol_version: 3,
                        negotiated_protocol_version: 3,
                        process_id: None,
                        startup: None,
                        available_modes: Vec::new(),
                        available_models: Vec::new(),
                    },
                    projects: Vec::new(),
                    failures: vec![session_manager::RestoreFailure {
                        app_session_id: "session-1".to_owned(),
                        sdk_session_id: "sdk-session-1".to_owned(),
                        error: "saved worktree is missing".to_owned(),
                    }],
                })
                .unwrap();

            view.update(cx, SessionMvpView::apply_service_updates);

            view.read_with(cx, |view, _| {
                let session = view.selected().expect("failed session remains selected");
                assert_eq!(session.snapshot.status, SessionStatus::Failed);
                assert_eq!(
                    session.snapshot.last_error.as_deref(),
                    Some("saved worktree is missing")
                );
            });
        }

        #[gpui::test]
        fn active_empty_composer_uses_the_trailing_action_to_cancel(cx: &mut TestAppContext) {
            let (view, cx, commands) = setup(cx);
            view.update(cx, |view, cx| {
                view.selected_session = Some("session-1".to_owned());
                let mut snapshot = (*view.sessions[0].snapshot).clone();
                snapshot.status = SessionStatus::Running;
                view.sessions[0].snapshot = Arc::new(snapshot);
                cx.notify();
            });
            cx.run_until_parked();

            assert!(cx.debug_bounds("stop-session").is_some());
            assert!(cx.debug_bounds("submit-prompt").is_none());
            assert!(cx.debug_bounds("close-session").is_none());

            let stop = cx
                .debug_bounds("stop-session")
                .expect("stop action rendered");
            cx.simulate_click(stop.center(), Modifiers::none());

            match commands.try_recv().expect("a command was sent") {
                ServiceCommand::Cancel { app_session_id } => {
                    assert_eq!(app_session_id, "session-1");
                }
                _ => panic!("expected a Cancel command"),
            }
        }

        #[gpui::test]
        fn typing_during_active_work_turns_stop_into_steering_send(cx: &mut TestAppContext) {
            let (view, cx, commands) = setup(cx);
            view.update(cx, |view, cx| {
                view.selected_session = Some("session-1".to_owned());
                let mut snapshot = (*view.sessions[0].snapshot).clone();
                snapshot.status = SessionStatus::Running;
                view.sessions[0].snapshot = Arc::new(snapshot);
                cx.notify();
            });
            cx.run_until_parked();

            let composer = view.read_with(cx, |view, _| view.composer.clone());
            composer.update(cx, |input, cx| input.set_value("change direction", cx));
            cx.run_until_parked();

            assert!(cx.debug_bounds("stop-session").is_none());
            let send = cx
                .debug_bounds("submit-prompt")
                .expect("steering send action rendered");
            cx.simulate_click(send.center(), Modifiers::none());

            match commands.try_recv().expect("a command was sent") {
                ServiceCommand::Submit {
                    app_session_id,
                    prompt,
                    ..
                } => {
                    assert_eq!(app_session_id.as_deref(), Some("session-1"));
                    assert_eq!(prompt, "change direction");
                }
                _ => panic!("expected a Submit command"),
            }
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

        /// A staged attachment travels with the prompt it was staged on.
        #[gpui::test]
        fn submitting_carries_staged_attachments(cx: &mut TestAppContext) {
            let (view, cx, commands) = setup(cx);
            view.update(cx, |view, _| {
                view.draft_attachments
                    .push(app_model::PromptAttachment::from_path(
                        std::path::Path::new("/tmp/shot.png"),
                    ));
            });

            view.update(cx, |view, _| view.submit_prompt("look".to_owned()));

            let mut attachments = None;
            while let Ok(command) = commands.try_recv() {
                if let ServiceCommand::Submit {
                    attachments: sent, ..
                } = command
                {
                    attachments = Some(sent);
                }
            }
            let attachments = attachments.expect("a submit command was sent");
            assert_eq!(attachments.len(), 1, "the staged screenshot was dropped");
            assert_eq!(attachments[0].identity(), "/tmp/shot.png");
            assert_eq!(attachments[0].display_name(), "shot.png");
        }

        /// Attachments belong to one prompt, not to every later prompt.
        #[gpui::test]
        fn attachments_do_not_repeat_on_the_next_prompt(cx: &mut TestAppContext) {
            let (view, cx, commands) = setup(cx);
            view.update(cx, |view, _| {
                view.draft_attachments
                    .push(app_model::PromptAttachment::from_path(
                        std::path::Path::new("/tmp/shot.png"),
                    ));
                view.submit_prompt("look".to_owned());
                view.submit_prompt("and now".to_owned());
            });

            let mut sends = Vec::new();
            while let Ok(command) = commands.try_recv() {
                if let ServiceCommand::Submit { attachments, .. } = command {
                    sends.push(attachments);
                }
            }
            assert_eq!(sends.len(), 2, "both prompts were sent");
            assert_eq!(sends[0].len(), 1);
            assert!(
                sends[1].is_empty(),
                "the screenshot was resent with an unrelated follow-up"
            );
        }

        /// An attachment on its own is a complete message.
        #[gpui::test]
        fn an_attachment_alone_can_be_submitted(cx: &mut TestAppContext) {
            let (view, cx, commands) = setup(cx);
            view.update(cx, |view, cx| {
                view.draft_attachments
                    .push(app_model::PromptAttachment::from_path(
                        std::path::Path::new("/tmp/shot.png"),
                    ));
                view.submit_composer(cx);
            });

            let sent = std::iter::from_fn(|| commands.try_recv().ok())
                .any(|command| matches!(command, ServiceCommand::Submit { .. }));
            assert!(sent, "an empty prompt with a screenshot sent nothing");
        }

        /// Removing an attachment takes it off the next prompt.
        #[gpui::test]
        fn removing_an_attachment_unstages_it(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                view.draft_attachments
                    .push(app_model::PromptAttachment::from_path(
                        std::path::Path::new("/tmp/shot.png"),
                    ));
                view.remove_attachment("/tmp/shot.png", cx);
            });
            view.update(cx, |view, _| {
                assert!(view.draft_attachments.is_empty());
            });
        }

        /// The chip strip only exists when something is attached.
        #[gpui::test]
        fn the_attachment_strip_appears_with_an_attachment(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            cx.run_until_parked();
            assert!(
                cx.debug_bounds("attachment-strip").is_none(),
                "the strip took up space with nothing attached"
            );

            view.update(cx, |view, cx| {
                view.draft_attachments
                    .push(app_model::PromptAttachment::from_path(
                        std::path::Path::new("/tmp/shot.png"),
                    ));
                cx.notify();
            });
            cx.run_until_parked();
            assert!(
                cx.debug_bounds("attachment-strip").is_some(),
                "the attached screenshot was never shown"
            );
        }

        /// A pasted screenshot has no path, so it must travel as bytes.
        #[gpui::test]
        fn pasting_an_image_stages_it_as_an_attachment(cx: &mut TestAppContext) {
            let (view, cx, commands) = setup(cx);
            view.update(cx, |view, cx| {
                view.attach_pasted_images(
                    &[super::super::PastedImage {
                        bytes: vec![0x89, 0x50, 0x4E, 0x47],
                        mime_type: "image/png".to_owned(),
                    }],
                    cx,
                );
                view.submit_prompt("what is wrong here".to_owned());
            });

            let mut attachments = None;
            while let Ok(command) = commands.try_recv() {
                if let ServiceCommand::Submit {
                    attachments: sent, ..
                } = command
                {
                    attachments = Some(sent);
                }
            }
            let attachments = attachments.expect("a submit command was sent");
            assert_eq!(attachments.len(), 1, "the pasted screenshot was dropped");
            let app_model::PromptAttachment::Image {
                mime_type, data, ..
            } = &attachments[0]
            else {
                panic!("a pasted image must travel as bytes, not as a path");
            };
            assert_eq!(mime_type, "image/png");
            // base64 of the PNG magic bytes, so the payload survived intact.
            assert_eq!(data, "iVBORw==");
        }

        /// Two pastes mean two images, even when the bytes are identical.
        #[gpui::test]
        fn pasting_twice_stages_two_images(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            let image = super::super::PastedImage {
                bytes: vec![1, 2, 3],
                mime_type: "image/png".to_owned(),
            };
            view.update(cx, |view, cx| {
                view.attach_pasted_images(std::slice::from_ref(&image), cx);
                view.attach_pasted_images(std::slice::from_ref(&image), cx);
            });
            view.update(cx, |view, _| {
                assert_eq!(
                    view.draft_attachments.len(),
                    2,
                    "the second paste was mistaken for a duplicate of the first"
                );
            });
        }

        /// Dropping files onto the composer stages them.
        #[gpui::test]
        fn dropping_files_stages_them(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                view.attach_dropped_paths(
                    &[
                        std::path::PathBuf::from("/tmp/one.png"),
                        std::path::PathBuf::from("/tmp/two.png"),
                    ],
                    cx,
                );
            });
            view.update(cx, |view, _| {
                assert_eq!(view.draft_attachments.len(), 2);
                assert_eq!(view.draft_attachments[0].display_name(), "one.png");
            });
        }

        /// Dropping the same file twice attaches it once.
        #[gpui::test]
        fn dropping_the_same_file_twice_stages_it_once(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            let paths = [std::path::PathBuf::from("/tmp/one.png")];
            view.update(cx, |view, cx| {
                view.attach_dropped_paths(&paths, cx);
                view.attach_dropped_paths(&paths, cx);
            });
            view.update(cx, |view, _| {
                assert_eq!(view.draft_attachments.len(), 1);
            });
        }

        /// Removing one pasted image must not remove its identical twin.
        #[gpui::test]
        fn removing_one_pasted_image_keeps_the_other(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            let image = super::super::PastedImage {
                bytes: vec![1, 2, 3],
                mime_type: "image/png".to_owned(),
            };
            view.update(cx, |view, cx| {
                view.attach_pasted_images(std::slice::from_ref(&image), cx);
                view.attach_pasted_images(std::slice::from_ref(&image), cx);
                let first = view.draft_attachments[0].identity();
                view.remove_attachment(&first, cx);
            });
            view.update(cx, |view, _| {
                assert_eq!(
                    view.draft_attachments.len(),
                    1,
                    "removing one image took its twin with it"
                );
                assert_eq!(view.draft_attachments[0].display_name(), "Pasted image 2");
            });
        }

        /// Paste was bound to cmd only, so on Linux and Windows the action
        /// never fired and a pasted screenshot vanished without a trace.
        #[gpui::test]
        fn pasting_an_image_with_the_platform_shortcut_attaches_it(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            cx.update(|_, cx| {
                cx.write_to_clipboard(gpui::ClipboardItem::new_image(&gpui::Image {
                    format: gpui::ImageFormat::Png,
                    bytes: vec![0x89, 0x50, 0x4E, 0x47],
                    id: 1,
                }));
            });
            view.update_in(cx, |view, window, cx| {
                let handle = gpui::Focusable::focus_handle(view.composer.read(cx), cx);
                window.focus(&handle, cx);
            });
            cx.run_until_parked();

            cx.simulate_keystrokes("secondary-v");
            cx.run_until_parked();

            view.update(cx, |view, _| {
                assert_eq!(
                    view.draft_attachments.len(),
                    1,
                    "the platform paste shortcut did not reach the composer"
                );
                assert!(view.draft_attachments[0].is_image());
            });
        }

        #[gpui::test]
        fn composer_wraps_text_to_multiple_lines(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            let single_line_height = cx
                .debug_bounds("composer-input")
                .expect("composer rendered")
                .size
                .height;

            view.update(cx, |view, cx| {
                view.composer.update(cx, |input, cx| {
                    input.set_value("word ".repeat(300), cx);
                });
            });
            cx.run_until_parked();

            let wrapped_height = cx
                .debug_bounds("composer-input")
                .expect("composer rendered")
                .size
                .height;
            assert!(
                wrapped_height > single_line_height * 2.,
                "long composer text remained on one line: {wrapped_height:?}"
            );
        }

        #[gpui::test]
        fn shift_enter_inserts_a_newline_without_submitting(cx: &mut TestAppContext) {
            let (view, cx, commands) = setup(cx);
            view.update_in(cx, |view, window, cx| {
                view.composer
                    .update(cx, |input, cx| input.set_value("first line", cx));
                let handle = gpui::Focusable::focus_handle(view.composer.read(cx), cx);
                window.focus(&handle, cx);
            });
            cx.run_until_parked();

            cx.simulate_keystrokes("shift-enter");
            cx.run_until_parked();

            view.read_with(cx, |view, cx| {
                assert_eq!(view.composer.read(cx).value(), "first line\n");
            });
            assert!(
                commands.try_recv().is_err(),
                "shift-enter submitted the composer"
            );
        }

        #[gpui::test]
        fn transcript_renders_markdown_and_copies_its_source(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            let source = "# Result\n\n| Name | State |\n|---|---|\n| Build | **Passing** |\n\n```rust\nfn main() {}\n```";
            view.update(cx, |view, cx| {
                let mut state = snapshot("session-1", "First session");
                state.transcript.push(app_model::TranscriptMessage {
                    id: "markdown-message".to_owned(),
                    role: app_model::TranscriptRole::Assistant,
                    content: source.to_owned(),
                    state: app_model::TranscriptState::Complete,
                    timestamp: "1".to_owned(),
                    sequence: 1,
                    attachments: Vec::new(),
                });
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(state))];
                view.selected_session = Some("session-1".to_owned());
                cx.notify();
            });
            cx.run_until_parked();

            assert!(cx.debug_bounds("markdown-content").is_some());
            assert!(cx.debug_bounds("markdown-table").is_some());
            assert!(cx.debug_bounds("markdown-code").is_some());

            let copy = cx
                .debug_bounds("copy-markdown")
                .expect("copy markdown button rendered");
            cx.simulate_click(copy.center(), Modifiers::none());
            assert_eq!(
                cx.read_from_clipboard().and_then(|item| item.text()),
                Some(source.to_owned())
            );
        }

        /// Clicking an image chip in the transcript shows the picture.
        #[gpui::test]
        fn clicking_a_transcript_image_opens_a_preview(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                let mut state = snapshot("session-1", "First session");
                state.transcript.push(app_model::TranscriptMessage {
                    id: "m1".to_owned(),
                    role: app_model::TranscriptRole::User,
                    content: "look".to_owned(),
                    state: app_model::TranscriptState::Complete,
                    timestamp: "1".to_owned(),
                    sequence: 1,
                    attachments: vec![app_model::MessageAttachment {
                        display_name: "Pasted Image".to_owned(),
                        is_image: true,
                        path: Some("/tmp/clipboard.png".to_owned()),
                    }],
                });
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(state))];
                view.selected_session = Some("session-1".to_owned());
                cx.notify();
            });
            cx.run_until_parked();

            let chip = cx
                .debug_bounds("message-attachment")
                .expect("the attachment chip rendered");
            cx.simulate_click(chip.center(), Modifiers::none());
            cx.run_until_parked();

            assert!(
                cx.debug_bounds("image-preview").is_some(),
                "clicking the image chip did not open a preview"
            );
        }

        /// The real sequence: click a chip, then press Escape. If opening the
        /// preview leaves focus outside the action's dispatch path, Escape is
        /// dead exactly when the user is most likely to reach for it.
        #[gpui::test]
        fn escape_closes_a_preview_opened_by_clicking(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                let mut state = snapshot("session-1", "First session");
                state.transcript.push(app_model::TranscriptMessage {
                    id: "m1".to_owned(),
                    role: app_model::TranscriptRole::User,
                    content: "look".to_owned(),
                    state: app_model::TranscriptState::Complete,
                    timestamp: "1".to_owned(),
                    sequence: 1,
                    attachments: vec![app_model::MessageAttachment {
                        display_name: "Pasted Image".to_owned(),
                        is_image: true,
                        path: Some("/tmp/clipboard.png".to_owned()),
                    }],
                });
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(state))];
                view.selected_session = Some("session-1".to_owned());
                cx.notify();
            });
            cx.run_until_parked();

            let chip = cx
                .debug_bounds("message-attachment")
                .expect("the attachment chip rendered");
            cx.simulate_click(chip.center(), Modifiers::none());
            cx.run_until_parked();
            assert!(cx.debug_bounds("image-preview").is_some());

            cx.simulate_keystrokes("escape");
            cx.run_until_parked();
            assert!(
                cx.debug_bounds("image-preview").is_none(),
                "escape did nothing after opening the preview by click"
            );
        }

        /// The preview closes without needing a specific target to hit.
        #[gpui::test]
        fn the_image_preview_closes_on_escape(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update_in(cx, |view, window, cx| {
                view.open_image_preview(
                    super::super::ImagePreview {
                        title: "Pasted Image".to_owned(),
                        source: super::super::PreviewSource::Path(std::path::PathBuf::from(
                            "/tmp/clipboard.png",
                        )),
                    },
                    window,
                    cx,
                );
            });
            cx.run_until_parked();
            assert!(cx.debug_bounds("image-preview").is_some());

            // Focus the composer first, mirroring a user who was typing.
            view.update_in(cx, |view, window, cx| {
                let handle = gpui::Focusable::focus_handle(view.composer.read(cx), cx);
                window.focus(&handle, cx);
            });
            cx.run_until_parked();
            cx.simulate_keystrokes("escape");
            cx.run_until_parked();
            assert!(
                cx.debug_bounds("image-preview").is_none(),
                "escape left the preview open"
            );
        }

        /// A non-image attachment has nothing to preview.
        #[gpui::test]
        fn a_non_image_attachment_does_not_open_a_preview(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                let mut state = snapshot("session-1", "First session");
                state.transcript.push(app_model::TranscriptMessage {
                    id: "m1".to_owned(),
                    role: app_model::TranscriptRole::User,
                    content: "look".to_owned(),
                    state: app_model::TranscriptState::Complete,
                    timestamp: "1".to_owned(),
                    sequence: 1,
                    attachments: vec![app_model::MessageAttachment {
                        display_name: "notes.txt".to_owned(),
                        is_image: false,
                        path: Some("/tmp/notes.txt".to_owned()),
                    }],
                });
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(state))];
                view.selected_session = Some("session-1".to_owned());
                cx.notify();
            });
            cx.run_until_parked();

            let chip = cx
                .debug_bounds("message-attachments")
                .expect("the attachment chip rendered");
            cx.simulate_click(chip.center(), Modifiers::none());
            cx.run_until_parked();

            assert!(
                cx.debug_bounds("image-preview").is_none(),
                "a text file was opened as a picture"
            );
        }

        /// The bug this replaces: pasted images were sent as inline blobs, and
        /// the runtime echoes an attachment back in the form it was sent. A
        /// blob has no path, so the transcript could never show the picture
        /// again. The earlier test fabricated a path that pasted images never
        /// actually receive, so it passed against a broken build.
        #[gpui::test]
        fn a_pasted_image_is_written_to_disk_and_sent_as_a_file(cx: &mut TestAppContext) {
            let (view, cx, commands, _attachments) = setup_with_attachments(cx);
            let png: Vec<u8> = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
            view.update(cx, |view, cx| {
                view.attach_pasted_images(
                    &[super::super::PastedImage {
                        bytes: png.clone(),
                        mime_type: "image/png".to_owned(),
                    }],
                    cx,
                );
                view.submit_prompt("look".to_owned());
            });

            let mut sent = None;
            while let Ok(command) = commands.try_recv() {
                if let ServiceCommand::Submit { attachments, .. } = command {
                    sent = Some(attachments);
                }
            }
            let sent = sent.expect("a submit command was sent");
            assert_eq!(sent.len(), 1);
            let path = sent[0]
                .path()
                .expect("a pasted image must be sent as a file, not an inline blob");
            assert_eq!(
                std::fs::read(path).expect("the image was written to disk"),
                png,
                "the file does not hold the pasted bytes"
            );
            assert!(
                std::path::Path::new(path)
                    .extension()
                    .is_some_and(|extension| extension == "png"),
                "the extension names the format"
            );
        }

        /// A pasted image is previewable from the composer before it is sent.
        #[gpui::test]
        fn a_pasted_image_can_be_previewed_before_sending(cx: &mut TestAppContext) {
            let (view, cx, _commands, _attachments) = setup_with_attachments(cx);
            view.update(cx, |view, cx| {
                view.attach_pasted_images(
                    &[super::super::PastedImage {
                        bytes: vec![0x89, 0x50, 0x4E, 0x47],
                        mime_type: "image/png".to_owned(),
                    }],
                    cx,
                );
            });
            view.update(cx, |view, _| {
                let preview = super::super::draft_preview(&view.draft_attachments[0])
                    .expect("a pasted image can be previewed");
                assert!(matches!(
                    preview.source,
                    super::super::PreviewSource::Path(_)
                ));
            });
        }

        /// A sent attachment is part of what was asked, so the transcript must
        /// still show it after the composer is cleared.
        #[gpui::test]
        fn a_sent_attachment_is_shown_in_the_transcript(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                let mut state = snapshot("session-1", "First session");
                state.transcript.push(app_model::TranscriptMessage {
                    id: "m1".to_owned(),
                    role: app_model::TranscriptRole::User,
                    content: "what is wrong here".to_owned(),
                    state: app_model::TranscriptState::Complete,
                    timestamp: "1".to_owned(),
                    sequence: 1,
                    attachments: vec![app_model::MessageAttachment {
                        display_name: "Pasted Image".to_owned(),
                        is_image: true,
                        path: None,
                    }],
                });
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(state))];
                view.selected_session = Some("session-1".to_owned());
                cx.notify();
            });
            cx.run_until_parked();

            assert!(
                cx.debug_bounds("message-attachments").is_some(),
                "the attachment vanished once the message was sent"
            );
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

        #[gpui::test]
        fn short_tool_entries_fill_the_composer_column(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                let mut state = snapshot("session-1", "First session");
                state.apply(app_model::DomainEvent::from_sdk_event_for(
                    "session-1",
                    1,
                    &serde_json::json!({"id":"t","type":"tool.execution_start",
                        "data":{"toolCallId":"c1","toolName":"grep",
                                "arguments":{"query":"x"}}}),
                ));
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(state))];
                view.selected_session = Some("session-1".to_owned());
                cx.notify();
            });
            cx.run_until_parked();

            let composer = cx.debug_bounds("composer").expect("composer rendered");
            let column = cx
                .debug_bounds("transcript-content")
                .expect("transcript column rendered");
            let tool = cx.debug_bounds("tool-card").expect("tool card rendered");
            assert_horizontally_aligned("transcript column", column, composer);
            assert_horizontally_aligned("short tool card", tool, composer);
        }

        #[gpui::test]
        fn wide_terminal_output_stays_inside_the_conversation_column(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                let mut state = snapshot("session-1", "First session");
                for (sequence, raw) in [
                    serde_json::json!({"id":"t","type":"tool.execution_start",
                        "data":{"toolCallId":"c1","toolName":"bash",
                                "arguments":{"command":"printf wide"},
                                "shellToolInfo":{"displayCommand":"printf wide",
                                                 "hasWriteFileRedirection":false,
                                                 "possiblePaths":[]}}}),
                    serde_json::json!({"id":"p","type":"tool.execution_partial_result",
                        "data":{"toolCallId":"c1","partialOutput":"x".repeat(4_000)}}),
                ]
                .into_iter()
                .enumerate()
                {
                    state.apply(app_model::DomainEvent::from_sdk_event_for(
                        "session-1",
                        u64::try_from(sequence).unwrap_or(0) + 1,
                        &raw,
                    ));
                }
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(state))];
                view.selected_session = Some("session-1".to_owned());
                cx.notify();
            });
            cx.run_until_parked();

            let column = cx
                .debug_bounds("transcript-content")
                .expect("transcript column rendered");
            let tool = cx.debug_bounds("tool-card").expect("tool card rendered");
            let output = cx
                .debug_bounds("tool-detail")
                .expect("terminal output rendered");
            assert_horizontally_aligned("terminal tool card", tool, column);
            assert!(
                output.origin.x >= tool.origin.x && output.right() <= tool.right(),
                "terminal output escaped its card: {output:?} vs {tool:?}"
            );
        }

        #[gpui::test]
        fn conversation_column_tracks_resizing_and_the_inspector(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                let mut state = snapshot("session-1", "First session");
                state.transcript.push(app_model::TranscriptMessage {
                    id: "assistant".to_owned(),
                    role: app_model::TranscriptRole::Assistant,
                    content: "Done".to_owned(),
                    state: app_model::TranscriptState::Complete,
                    timestamp: "1".to_owned(),
                    sequence: 1,
                    attachments: Vec::new(),
                });
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(state))];
                view.selected_session = Some("session-1".to_owned());
                cx.notify();
            });
            cx.simulate_resize(gpui::size(gpui::px(1_400.0), gpui::px(800.0)));
            cx.run_until_parked();

            let wide_composer = cx.debug_bounds("composer").expect("composer rendered");
            let wide_message = cx
                .debug_bounds("transcript-message")
                .expect("message rendered");
            assert_horizontally_aligned("wide message", wide_message, wide_composer);
            assert_eq!(
                wide_composer.size.width,
                gpui::px(super::super::CONVERSATION_COLUMN_WIDTH)
            );

            view.update(cx, |view, cx| {
                view.panel_open = true;
                cx.notify();
            });
            cx.run_until_parked();
            let inspected_composer = cx.debug_bounds("composer").expect("composer rendered");
            let inspected_message = cx
                .debug_bounds("transcript-message")
                .expect("message rendered");
            assert_horizontally_aligned(
                "message with inspector open",
                inspected_message,
                inspected_composer,
            );

            view.update(cx, |view, cx| {
                view.panel_open = false;
                cx.notify();
            });
            cx.simulate_resize(gpui::size(gpui::px(800.0), gpui::px(800.0)));
            cx.run_until_parked();
            let compact_composer = cx.debug_bounds("composer").expect("composer rendered");
            let compact_message = cx
                .debug_bounds("transcript-message")
                .expect("message rendered");
            assert_horizontally_aligned("compact message", compact_message, compact_composer);
            assert!(
                compact_composer.size.width < wide_composer.size.width,
                "the column did not respond to the narrower window"
            );
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

        /// Regression: the thumb was drawn but inert, so the wheel was the only
        /// way to move a scrollable region.
        #[gpui::test]
        fn dragging_the_transcript_scrollbar_scrolls_it(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                let mut state = snapshot("session-1", "First session");
                for index in 0..120 {
                    state.transcript.push(app_model::TranscriptMessage {
                        id: format!("m{index}"),
                        role: app_model::TranscriptRole::Assistant,
                        content: format!("message {index} with enough text to take a line"),
                        state: app_model::TranscriptState::Complete,
                        timestamp: "1".to_owned(),
                        sequence: u64::try_from(index).unwrap_or(0) + 1,
                        attachments: Vec::new(),
                    });
                }
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(state))];
                view.selected_session = Some("session-1".to_owned());
                cx.notify();
            });
            cx.run_until_parked();
            // Scrollbar geometry needs a measured layout, so give it the
            // follow-up frame the extent change requests.
            view.update(cx, |_, cx| cx.notify());
            cx.run_until_parked();

            // Auto-follow leaves the view at the bottom.
            let bottom = view.read_with(cx, |view, _| view.transcript_scroll.offset().y);
            assert!(bottom < gpui::px(0.0));

            // Press near the top of the track: the content should jump up.
            let track = cx.debug_bounds("scrollbar").expect("scrollbar rendered");
            let track_x = track.center().x;
            let near_top = track.origin.y + gpui::px(10.0);
            cx.simulate_mouse_down(
                gpui::point(track_x, near_top),
                MouseButton::Left,
                Modifiers::none(),
            );
            cx.run_until_parked();

            let after = view.read_with(cx, |view, _| view.transcript_scroll.offset().y);
            assert!(
                after > bottom,
                "dragging the scrollbar must scroll: {bottom:?} -> {after:?}"
            );
            view.read_with(cx, |view, _| {
                assert_eq!(
                    view.dragging_scrollbar
                        .as_ref()
                        .map(|drag| drag.id.as_str()),
                    Some(super::super::TRANSCRIPT_SCROLL_ID),
                    "the press should begin a drag"
                );
            });

            // Releasing ends the drag so later moves do not keep scrolling.
            cx.simulate_mouse_up(
                gpui::point(track_x, near_top),
                MouseButton::Left,
                Modifiers::none(),
            );
            cx.run_until_parked();
            view.read_with(cx, |view, _| {
                assert!(view.dragging_scrollbar.is_none());
            });
        }

        /// Regression: the thumb sits above the track and swallowed presses, so
        /// it could only be grabbed by clicking the sliver of track beside it.
        #[gpui::test]
        fn the_scrollbar_thumb_itself_can_be_grabbed(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                let mut state = snapshot("session-1", "First session");
                for index in 0..120 {
                    state.transcript.push(app_model::TranscriptMessage {
                        id: format!("m{index}"),
                        role: app_model::TranscriptRole::Assistant,
                        content: format!("message {index} with enough text to take a line"),
                        state: app_model::TranscriptState::Complete,
                        timestamp: "1".to_owned(),
                        sequence: u64::try_from(index).unwrap_or(0) + 1,
                        attachments: Vec::new(),
                    });
                }
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(state))];
                view.selected_session = Some("session-1".to_owned());
                cx.notify();
            });
            cx.run_until_parked();
            view.update(cx, |_, cx| cx.notify());
            cx.run_until_parked();

            // Auto-follow leaves the thumb at the bottom of its track.
            let handle = view.read_with(cx, |view, _| view.transcript_scroll.clone());
            let geometry = super::super::SessionMvpView::scrollbar_geometry(&handle)
                .expect("the transcript should be scrollable");
            let track = cx.debug_bounds("scrollbar").expect("scrollbar rendered");

            // Press the middle of the thumb itself, not the track beside it.
            let thumb_middle =
                geometry.track_top + gpui::px(geometry.thumb_top + geometry.thumb / 2.0);
            cx.simulate_mouse_down(
                gpui::point(track.center().x, thumb_middle),
                MouseButton::Left,
                Modifiers::none(),
            );
            cx.run_until_parked();

            view.read_with(cx, |view, _| {
                assert!(
                    view.dragging_scrollbar.is_some(),
                    "pressing the thumb must start a drag"
                );
            });

            // Grabbing the middle of the thumb must not move the content; the
            // grab point is preserved rather than recentred on the pointer.
            let after = view.read_with(cx, |view, _| view.transcript_scroll.offset().y);
            let expected = -(geometry.thumb_top / geometry.usable * geometry.scrollable);
            assert!(
                (f32::from(after) - expected).abs() < 2.0,
                "grabbing the thumb should not lurch: {after:?} vs {expected}"
            );
        }

        /// Regression: the thumb was drawn from one calculation and hit-tested
        /// against another, so pressing the visible thumb was often treated as
        /// pressing bare track. Where it is drawn must be where it is grabbed.
        #[gpui::test]
        fn the_drawn_thumb_matches_the_grabbable_thumb(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                let mut state = snapshot("session-1", "First session");
                // Enough content that the thumb is small, which is where the
                // two calculations diverged.
                for index in 0..400 {
                    state.transcript.push(app_model::TranscriptMessage {
                        id: format!("m{index}"),
                        role: app_model::TranscriptRole::Assistant,
                        content: format!("message {index} with enough text to take a line"),
                        state: app_model::TranscriptState::Complete,
                        timestamp: "1".to_owned(),
                        sequence: u64::try_from(index).unwrap_or(0) + 1,
                        attachments: Vec::new(),
                    });
                }
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(state))];
                view.selected_session = Some("session-1".to_owned());
                cx.notify();
            });
            cx.run_until_parked();
            view.update(cx, |_, cx| cx.notify());
            cx.run_until_parked();

            let handle = view.read_with(cx, |view, _| view.transcript_scroll.clone());
            let geometry = super::super::SessionMvpView::scrollbar_geometry(&handle)
                .expect("the transcript should be scrollable");
            let drawn = cx.debug_bounds("scrollbar-thumb").expect("thumb rendered");

            let drawn_top = f32::from(drawn.origin.y - geometry.track_top);
            assert!(
                (drawn_top - geometry.thumb_top).abs() < 1.0,
                "thumb drawn at {drawn_top} but grabbable at {}",
                geometry.thumb_top
            );
            assert!(
                (f32::from(drawn.size.height) - geometry.thumb).abs() < 1.0,
                "thumb drawn {:?} tall but grabbable {} tall",
                drawn.size.height,
                geometry.thumb
            );

            // Pressing the drawn thumb's middle must therefore grab it, not
            // jump the content.
            let before = view.read_with(cx, |view, _| view.transcript_scroll.offset().y);
            cx.simulate_mouse_down(drawn.center(), MouseButton::Left, Modifiers::none());
            cx.run_until_parked();
            let after = view.read_with(cx, |view, _| view.transcript_scroll.offset().y);
            assert!(
                (f32::from(after) - f32::from(before)).abs() < 4.0,
                "pressing the drawn thumb must grab it: {before:?} -> {after:?}"
            );
        }

        /// Regression: the drag recentred the thumb on the pointer, so grabbing
        /// it anywhere but the exact middle made the content jump before the
        /// drag had moved at all.
        #[gpui::test]
        fn grabbing_the_thumb_off_centre_does_not_jump(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                let mut state = snapshot("session-1", "First session");
                for index in 0..120 {
                    state.transcript.push(app_model::TranscriptMessage {
                        id: format!("m{index}"),
                        role: app_model::TranscriptRole::Assistant,
                        content: format!("message {index} with enough text to take a line"),
                        state: app_model::TranscriptState::Complete,
                        timestamp: "1".to_owned(),
                        sequence: u64::try_from(index).unwrap_or(0) + 1,
                        attachments: Vec::new(),
                    });
                }
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(state))];
                view.selected_session = Some("session-1".to_owned());
                cx.notify();
            });
            cx.run_until_parked();
            view.update(cx, |_, cx| cx.notify());
            cx.run_until_parked();

            // Move off the bottom so the thumb has room either side of it.
            view.update(cx, |view, _| {
                view.drag_scrollbar_to(
                    super::super::TRANSCRIPT_SCROLL_ID,
                    view.transcript_scroll.bounds().origin.y + gpui::px(200.0),
                    0.0,
                );
            });
            cx.run_until_parked();

            let before = view.read_with(cx, |view, _| view.transcript_scroll.offset().y);
            let handle = view.read_with(cx, |view, _| view.transcript_scroll.clone());
            let geometry =
                super::super::SessionMvpView::scrollbar_geometry(&handle).expect("scrollable");
            let track = cx.debug_bounds("scrollbar").expect("scrollbar rendered");

            // Press near the top edge of the thumb rather than its centre.
            let near_thumb_top = geometry.track_top + gpui::px(geometry.thumb_top + 2.0);
            cx.simulate_mouse_down(
                gpui::point(track.center().x, near_thumb_top),
                MouseButton::Left,
                Modifiers::none(),
            );
            cx.run_until_parked();

            let after = view.read_with(cx, |view, _| view.transcript_scroll.offset().y);
            assert!(
                (f32::from(after) - f32::from(before)).abs() < 4.0,
                "pressing the thumb must not move the content: {before:?} -> {after:?}"
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

        /// `read_bash` carries output from a long-running shell. Treating it as
        /// control-only hid every compile chunk until the agent finished.
        #[gpui::test]
        fn running_read_bash_output_streams_in_the_transcript(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                let mut state = snapshot("session-1", "First session");
                for (sequence, raw) in [
                    serde_json::json!({"id":"t","type":"tool.execution_start",
                        "data":{"toolCallId":"c1","toolName":"read_bash",
                                "arguments":{"shellId":"36"}}}),
                    serde_json::json!({"id":"p","type":"tool.execution_partial_result",
                        "data":{"toolCallId":"c1","partialOutput":"Compiling gcabb v0.1.0\n"}}),
                ]
                .into_iter()
                .enumerate()
                {
                    state.apply(app_model::DomainEvent::from_sdk_event_for(
                        "session-1",
                        u64::try_from(sequence).unwrap_or(0) + 1,
                        &raw,
                    ));
                }
                view.sessions = vec![SessionProjection::for_test(SessionHandle::for_test(state))];
                view.selected_session = Some("session-1".to_owned());
                cx.notify();
            });
            cx.run_until_parked();

            assert!(
                cx.debug_bounds("tool-detail").is_some(),
                "partial read_bash output should render before the shell completes"
            );
            view.read_with(cx, |view, _| {
                assert_eq!(view.transcript_extent.3, 1);
                assert_eq!(view.transcript_extent.4, "Compiling gcabb v0.1.0\n".len());
            });
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
                        attachments: Vec::new(),
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
                    attachments: Vec::new(),
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

        /// An install with nothing to report must not lose space to a banner.
        #[gpui::test]
        fn no_banner_is_shown_when_there_is_no_update(cx: &mut TestAppContext) {
            let (_view, cx, _commands) = setup(cx);
            assert!(
                cx.debug_bounds("update-banner").is_none(),
                "the banner must stay hidden when there is no update"
            );
        }

        #[gpui::test]
        fn an_offered_update_shows_the_banner_with_its_version(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                view.update_ui = UpdateUi::Available {
                    version: "0.2.0".to_owned(),
                    notes: "## GCABB v0.2.0\n\nFaster startup.".to_owned(),
                };
                cx.notify();
            });
            cx.run_until_parked();

            assert!(
                cx.debug_bounds("update-banner").is_some(),
                "an offered update must be visible"
            );
            view.read_with(cx, |view, _| {
                let (message, _, summary) =
                    view.update_banner_text().expect("banner text rendered");
                assert!(message.contains("0.2.0"), "got {message}");
                // The heading is skipped so the summary is the first real line.
                assert_eq!(summary.as_deref(), Some("Faster startup."));
            });
        }

        /// Regression guard: without a worker the buttons must still be inert
        /// rather than panicking, since a developer build has no worker at all.
        #[gpui::test]
        fn pressing_update_without_a_worker_is_harmless(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                view.update_ui = UpdateUi::Available {
                    version: "0.2.0".to_owned(),
                    notes: String::new(),
                };
                cx.notify();
            });
            cx.run_until_parked();

            let button = cx.debug_bounds("Update").expect("update button rendered");
            cx.simulate_click(button.center(), Modifiers::none());
            cx.run_until_parked();

            view.read_with(cx, |view, _| {
                assert!(view.update_service.is_none(), "test builds have no worker");
                assert!(matches!(view.update_ui, UpdateUi::Available { .. }));
            });
        }

        /// A failed update must be dismissible, or the banner would be stuck.
        #[gpui::test]
        fn a_failed_update_can_be_dismissed(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                view.update_ui = UpdateUi::Failed("signature does not match".to_owned());
                cx.notify();
            });
            cx.run_until_parked();

            let button = cx.debug_bounds("Dismiss").expect("dismiss button rendered");
            cx.simulate_click(button.center(), Modifiers::none());
            cx.run_until_parked();

            view.read_with(cx, |view, _| assert_eq!(view.update_ui, UpdateUi::Hidden));
            assert!(
                cx.debug_bounds("update-banner").is_none(),
                "the banner goes away once dismissed"
            );
        }

        /// An applied update takes effect on restart, so it must say so and
        /// offer the restart rather than silently doing nothing.
        #[gpui::test]
        fn an_applied_update_offers_a_restart(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                view.update_ui = UpdateUi::ReadyToRestart {
                    version: "0.2.0".to_owned(),
                };
                cx.notify();
            });
            cx.run_until_parked();

            assert!(
                cx.debug_bounds("update-restart").is_some(),
                "a staged update must offer a restart"
            );
            view.read_with(cx, |view, _| {
                let (message, _, _) = view.update_banner_text().expect("banner text rendered");
                assert!(message.contains("restart"), "got {message}");
            });
        }

        #[gpui::test]
        fn download_progress_is_shown_in_the_banner(cx: &mut TestAppContext) {
            let (view, cx, _commands) = setup(cx);
            view.update(cx, |view, cx| {
                view.update_ui = UpdateUi::Downloading {
                    received: 512,
                    total: Some(1024),
                };
                cx.notify();
            });
            cx.run_until_parked();

            view.read_with(cx, |view, _| {
                let (message, _, _) = view.update_banner_text().expect("banner text rendered");
                assert!(message.contains("50%"), "got {message}");
            });
        }
    }
}
