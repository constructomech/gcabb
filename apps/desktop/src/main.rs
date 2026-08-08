use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use app_model::{
    ContextWindowOption, InteractionKind, InteractionResponse, ProjectMetadata, SessionSnapshot,
    SessionStatus, TranscriptRole, TranscriptState,
};
use copilot_provider::{CopilotProvider, ProviderCompatibility};
use diagnostics::{TracingDiagnostics, init_tracing};
use gpui::prelude::FluentBuilder;
use gpui::{
    App, AppContext, Bounds, Context, Entity, Focusable, InteractiveElement, IntoElement,
    KeyBinding, MouseButton, ParentElement, Render, Role, SharedString, StatefulInteractiveElement,
    Styled, TitlebarOptions, Window, WindowBounds, WindowOptions, actions, div, px, rgb, size,
};
use session_manager::{CreateSessionRequest, RestoreFailure, SessionHandle, SessionManager};
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
                let project = ProjectMetadata {
                    id: project_root.to_string_lossy().into_owned(),
                    path: project_root.to_string_lossy().into_owned(),
                    name: project_root
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("Project")
                        .to_owned(),
                    default_branch: Some(git_branch(&project_root)),
                    last_opened_at: timestamp(),
                };
                if let Err(error) = manager.register_project(&project) {
                    let _ = update_tx.send(ServiceUpdate::Failed(format!(
                        "failed to register project: {error}"
                    )));
                    let _ = stopped_tx.send(());
                    return;
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
                    let is_submit = matches!(&command, ServiceCommand::Submit { .. });
                    match runtime.block_on(handle_service_command(&manager, command)) {
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
                            let _ = update_tx.send(ServiceUpdate::SessionsDiscovered(sessions));
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
}

#[allow(clippy::too_many_lines)]
async fn handle_service_command(
    manager: &SessionManager,
    command: ServiceCommand,
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
        } => {
            let handle = if let Some(id) = app_session_id {
                manager
                    .session(&id)
                    .await
                    .map_err(|error| error.to_string())?
            } else {
                let initial_mode = mode.clone();
                let handle = manager
                    .create_session(CreateSessionRequest {
                        project_path,
                        title: session_title(&prompt),
                        model,
                        mode: Some(mode),
                        reasoning_effort,
                        context_tier,
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
        ServiceCommand::Select { app_session_id } => manager
            .set_selected_session(app_session_id.as_deref())
            .map_err(|error| error.to_string())?,
        ServiceCommand::Stop => {}
    }
    Ok(created)
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
}

enum StartupState {
    Starting,
    Ready(ProviderCompatibility),
    Failed(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ControlMenu {
    Mode,
    Model,
    Effort,
    Context,
}

struct SessionMvpView {
    startup: StartupState,
    projects: Vec<ProjectMetadata>,
    sessions: Vec<SessionProjection>,
    selected_session: Option<String>,
    selected_project: PathBuf,
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
    open_control_menu: Option<ControlMenu>,
    action_error: Option<String>,
    _poll_task: gpui::Task<()>,
}

impl SessionMvpView {
    fn new(
        service: AppService,
        project_root: PathBuf,
        branch: String,
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
            selected_project: project_root,
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
            open_control_menu: None,
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
                    if let Some(project_path) = self
                        .selected()
                        .map(|session| PathBuf::from(&session.snapshot.metadata.project_path))
                    {
                        self.selected_project = project_path;
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
                ServiceUpdate::PromptAccepted => {
                    self.composer.update(cx, TextInput::clear);
                }
                ServiceUpdate::ActionFailed(error) => self.action_error = Some(error),
                ServiceUpdate::Failed(error) => self.startup = StartupState::Failed(error),
            }
        }
        changed
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
        let _ = self.commands.send(ServiceCommand::Submit {
            app_session_id: self.selected_session.clone(),
            prompt,
            project_path: self.selected_project.clone(),
            model: self.draft_model.clone(),
            mode: self.draft_mode.clone(),
            reasoning_effort: reasoning_effort_for_model(&supported_efforts, &self.draft_effort),
            context_tier: self.selectable_context_tier(),
        });
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
            if let Some(project_path) = self
                .selected()
                .map(|session| PathBuf::from(&session.snapshot.metadata.project_path))
            {
                self.selected_project = project_path;
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
        self.selected_project = PathBuf::from(path);
        self.selected_session = self
            .sessions
            .iter()
            .find(|session| session.snapshot.metadata.project_path == path)
            .map(|session| session.handle.id().to_owned());
        let _ = self.commands.send(ServiceCommand::Select {
            app_session_id: self.selected_session.clone(),
        });
        cx.notify();
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

    fn choose_control(&mut self, menu: ControlMenu, value: String) {
        match menu {
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
        self.selected()
            .and_then(|session| {
                session
                    .snapshot
                    .controls
                    .available_models
                    .iter()
                    .find(|model| model.id == model_id)
            })
            .or_else(|| match &self.startup {
                StartupState::Ready(compatibility) => compatibility
                    .available_models
                    .iter()
                    .find(|model| model.id == model_id),
                StartupState::Starting | StartupState::Failed(_) => None,
            })
            .map_or_else(Vec::new, |model| model.supported_reasoning_efforts.clone())
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
        self.selected()
            .and_then(|session| {
                session
                    .snapshot
                    .controls
                    .available_models
                    .iter()
                    .find(|model| model.id == model_id)
            })
            .or_else(|| match &self.startup {
                StartupState::Ready(compatibility) => compatibility
                    .available_models
                    .iter()
                    .find(|model| model.id == model_id),
                StartupState::Starting | StartupState::Failed(_) => None,
            })
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
                                view.choose_control(menu, option_value.clone());
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
            .filter(|session| session.snapshot.metadata.project_path == selected_path)
            .map(|session| {
                let id = session.handle.id().to_owned();
                let accessible_id = id.clone();
                let label = session.snapshot.metadata.title.clone();
                let selected = self.selected_session.as_deref() == Some(id.as_str());
                div()
                    .id(SharedString::from(format!("session-{id}")))
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
        let projects = self.projects.iter().map(|project| {
            let path = project.path.clone();
            let selected = project.path == selected_path;
            let label = project.name.clone();
            div()
                .id(SharedString::from(format!("project-{path}")))
                .accessibility_id(path.clone())
                .role(Role::ListItem)
                .aria_label(label)
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
                .child(project.name.clone())
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
                            .aria_label("Chats home")
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
                            .text_color(rgb(MUTED))
                            .child("◯")
                            .child("Chats")
                            .hover(|style| style.bg(rgb(ELEVATED)).cursor_pointer())
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.new_session(cx);
                            })),
                    )
                    .children(projects)
                    .children(sessions),
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
        let messages = session
            .snapshot
            .transcript
            .iter()
            .rev()
            .take(60)
            .rev()
            .map(|message| {
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
            });
        div()
            .id("transcript")
            .role(Role::List)
            .aria_label("Conversation")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .p_5()
            .gap_3()
            .overflow_hidden()
            .children(messages)
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
        let project_name = self
            .projects
            .iter()
            .find(|project| Path::new(&project.path) == self.selected_project)
            .map_or_else(
                || {
                    self.selected_project
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("Project")
                        .to_owned()
                },
                |project| project.name.clone(),
            );
        let branch = self
            .projects
            .iter()
            .find(|project| Path::new(&project.path) == self.selected_project)
            .and_then(|project| project.default_branch.clone())
            .unwrap_or_else(|| self.branch.clone());
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
                    .text_sm()
                    .text_color(rgb(MUTED))
                    .child(format!("▱ {project_name}"))
                    .child("↗ Current checkout")
                    .child(format!("⌁ {branch}"))
                    .child(div().flex_1())
                    .child(
                        div()
                            .id("add-project-placeholder")
                            .child("+ Add project — unavailable"),
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
        let branch = self
            .projects
            .iter()
            .find(|project| Path::new(&project.path) == self.selected_project)
            .and_then(|project| project.default_branch.clone())
            .unwrap_or_else(|| self.branch.clone());
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
                                        self.selected_project.display(),
                                        branch
                                    )),
                                ))
                                .child(
                                    div()
                                        .id("provider-status")
                                        .role(Role::Status)
                                        .aria_label(provider_status.clone())
                                        .text_xs()
                                        .text_color(rgb(provider_color))
                                        .child(provider_status),
                                ),
                        )
                    })
                    .when(self.selected_session.is_some(), |main| {
                        main.child(self.transcript())
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
            .when_some(self.interaction_dialog(cx), |root, dialog| {
                root.child(dialog)
            })
    }
}

fn control_pill(
    id: &'static str,
    value: String,
    menu: ControlMenu,
    expanded: bool,
    cx: &mut Context<SessionMvpView>,
) -> impl IntoElement {
    let label = match menu {
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

fn prepare_data_directory(path: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(path)
        .map_err(|error| format!("failed to create {}: {error}", path.display()))?;
    Ok(path.join("gcabb.db"))
}

fn git_branch(root: &Path) -> String {
    std::process::Command::new("git")
        .current_dir(root)
        .args(["branch", "--show-current"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|branch| !branch.is_empty())
        .unwrap_or_else(|| "detached".to_owned())
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
                move |_, cx| cx.new(|cx| SessionMvpView::new(service, project_root, branch, cx)),
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
        control_menu_offset, default_context_tier, effort_label, reasoning_effort_for_model,
        toggled_menu, token_label,
    };

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
}
