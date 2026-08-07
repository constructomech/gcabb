use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use app_model::{
    InteractionKind, InteractionResponse, ProjectMetadata, SessionSnapshot, SessionStatus,
    TranscriptRole, TranscriptState,
};
use copilot_provider::{CopilotProvider, ProviderCompatibility};
use diagnostics::{TracingDiagnostics, init_tracing};
use gpui::prelude::FluentBuilder;
use gpui::{
    App, AppContext, Bounds, Context, Entity, Focusable, InteractiveElement, IntoElement,
    KeyBinding, ParentElement, Render, Role, SharedString, StatefulInteractiveElement, Styled,
    Window, WindowBounds, WindowOptions, actions, div, px, rgb, size,
};
use session_manager::{CreateSessionRequest, RestoreFailure, SessionHandle, SessionManager};
use storage::Storage;
use tokio::sync::watch;
use ui_components::{InputSubmitted, TextInput, bind_text_input_keys};

const BACKGROUND: u32 = 0x0012_1419;
const SIDEBAR: u32 = 0x0018_1b21;
const PANEL: u32 = 0x001e_222a;
const ELEVATED: u32 = 0x0027_2c35;
const BORDER: u32 = 0x0034_3a46;
const PRIMARY: u32 = 0x00ef_f2f7;
const MUTED: u32 = 0x0095_9eae;
const GREEN: u32 = 0x0068_d391;
const BLUE: u32 = 0x006f_a8ff;
const AMBER: u32 = 0x00e2_b76a;
const RED: u32 = 0x00ed_6a72;

actions!(gcabb, [FocusNext, FocusPrevious]);

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
        reasoning_effort: String,
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
    },
    SetMode {
        app_session_id: String,
        mode: String,
    },
    SetReasoningEffort {
        app_session_id: String,
        effort: String,
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
                        reasoning_effort: Some(reasoning_effort),
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
        } => manager
            .session(&app_session_id)
            .await
            .map_err(|error| error.to_string())?
            .set_model(model)
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
                "Ask Copilot to work on this project...",
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
                        view.apply_service_updates(cx);
                        view.refresh_snapshots();
                        cx.notify();
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
            action_error: None,
            _poll_task: poll_task,
        }
    }

    fn apply_service_updates(&mut self, cx: &mut Context<Self>) {
        loop {
            match self.updates.try_recv() {
                Ok(ServiceUpdate::Ready {
                    compatibility,
                    projects,
                    restored,
                    failures,
                    selected_session,
                }) => {
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
                Ok(ServiceUpdate::SessionAdded(handle)) => {
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
                Ok(ServiceUpdate::SessionsDiscovered(handles)) => {
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
                Ok(ServiceUpdate::PromptAccepted) => {
                    self.composer.update(cx, TextInput::clear);
                }
                Ok(ServiceUpdate::ActionFailed(error)) => self.action_error = Some(error),
                Ok(ServiceUpdate::Failed(error)) => self.startup = StartupState::Failed(error),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
    }

    fn refresh_snapshots(&mut self) {
        for projection in &mut self.sessions {
            if projection.receiver.has_changed().unwrap_or(false) {
                projection.snapshot = projection.receiver.borrow_and_update().clone();
            }
        }
    }

    fn selected(&self) -> Option<&SessionProjection> {
        let id = self.selected_session.as_deref()?;
        self.sessions
            .iter()
            .find(|session| session.handle.id() == id)
    }

    fn submit_prompt(&mut self, prompt: String) {
        self.action_error = None;
        let _ = self.commands.send(ServiceCommand::Submit {
            app_session_id: self.selected_session.clone(),
            prompt,
            project_path: self.selected_project.clone(),
            model: self.draft_model.clone(),
            mode: self.draft_mode.clone(),
            reasoning_effort: self.draft_effort.clone(),
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
        }
        cx.notify();
    }

    fn select_project(&mut self, path: &str, cx: &mut Context<Self>) {
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
        self.selected_session = None;
        let _ = self.commands.send(ServiceCommand::Select {
            app_session_id: None,
        });
        self.action_error = None;
        cx.notify();
    }

    fn cycle_mode(&mut self) {
        let next = match self.draft_mode.as_str() {
            "interactive" => "plan",
            "plan" => "autopilot",
            _ => "interactive",
        };
        next.clone_into(&mut self.draft_mode);
        if let Some(id) = self.selected_session.clone() {
            let _ = self.commands.send(ServiceCommand::SetMode {
                app_session_id: id,
                mode: self.draft_mode.clone(),
            });
        }
    }

    fn cycle_effort(&mut self) {
        let next = match self.draft_effort.as_str() {
            "low" => "medium",
            "medium" => "high",
            "high" => "xhigh",
            _ => "low",
        };
        next.clone_into(&mut self.draft_effort);
        if let Some(id) = self.selected_session.clone() {
            let _ = self.commands.send(ServiceCommand::SetReasoningEffort {
                app_session_id: id,
                effort: self.draft_effort.clone(),
            });
        }
    }

    fn cycle_model(&mut self) {
        let models = self.selected().map_or_else(Vec::new, |session| {
            session.snapshot.controls.available_models.clone()
        });
        if models.is_empty() {
            return;
        }
        let current = self.draft_model.as_deref();
        let index = models
            .iter()
            .position(|model| Some(model.id.as_str()) == current)
            .map_or(0, |index| (index + 1) % models.len());
        self.draft_model = Some(models[index].id.clone());
        if let Some(id) = self.selected_session.clone() {
            let _ = self.commands.send(ServiceCommand::SetModel {
                app_session_id: id,
                model: models[index].id.clone(),
            });
        }
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

    #[allow(clippy::too_many_lines)]
    fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let selected_path = self.selected_project.to_string_lossy();
        let sessions = self
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, session)| session.snapshot.metadata.project_path == selected_path)
            .map(|(_index, session)| {
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
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_3()
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
                            .text_sm()
                            .text_color(rgb(PRIMARY))
                            .child(session.snapshot.metadata.title.clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(status_color(session.snapshot.status))
                            .child(format!(
                                "{:?} · {} messages",
                                session.snapshot.status,
                                session.snapshot.transcript.len()
                            )),
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
                .px_2()
                .py_1()
                .rounded_md()
                .text_sm()
                .text_color(if selected { rgb(PRIMARY) } else { rgb(MUTED) })
                .bg(if selected {
                    rgb(ELEVATED)
                } else {
                    rgb(SIDEBAR)
                })
                .child(project.name.clone())
                .hover(|style| style.bg(rgb(ELEVATED)).cursor_pointer())
                .on_click(cx.listener(move |view, _, _, cx| {
                    view.select_project(&path, cx);
                }))
        });
        let new_button = div()
            .id("new-session")
            .accessibility_id("new-session")
            .role(Role::Button)
            .aria_label("New session")
            .focusable()
            .tab_stop(true)
            .p_2()
            .rounded_md()
            .bg(rgb(BLUE))
            .text_color(rgb(BACKGROUND))
            .text_sm()
            .child("+ New session")
            .hover(|style| style.opacity(0.85).cursor_pointer())
            .on_click(cx.listener(|view, _, _, cx| view.new_session(cx)));
        div()
            .id("sidebar")
            .accessibility_id("sidebar")
            .role(Role::Navigation)
            .aria_label("Projects and sessions")
            .flex()
            .flex_col()
            .w(px(286.0))
            .h_full()
            .bg(rgb(SIDEBAR))
            .border_r_1()
            .border_color(rgb(BORDER))
            .p_3()
            .gap_3()
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_center()
                    .child(
                        div()
                            .id("application-name")
                            .role(Role::Heading)
                            .aria_level(1)
                            .aria_label("GCABB")
                            .font_weight(gpui::FontWeight::BOLD)
                            .child("GCABB"),
                    )
                    .child(new_button),
            )
            .child(
                div()
                    .id("project-list")
                    .role(Role::List)
                    .aria_label("Projects")
                    .flex()
                    .flex_col()
                    .gap_1()
                    .children(projects),
            )
            .child(
                div()
                    .id("session-list")
                    .role(Role::List)
                    .aria_label("Sessions")
                    .flex()
                    .flex_col()
                    .gap_1()
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
                                .child("Start a new isolated coding session in this project."),
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
    fn composer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mode = self.draft_mode.clone();
        let effort = self.draft_effort.clone();
        let model = self
            .draft_model
            .clone()
            .unwrap_or_else(|| "Auto model".to_owned());
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
            .role(Role::Group)
            .aria_label("Message composer")
            .mx_auto()
            .mb_4()
            .w(px(820.0))
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
                    .child(control_pill("mode", mode, cx, SessionMvpView::cycle_mode))
                    .child(control_pill(
                        "model",
                        model,
                        cx,
                        SessionMvpView::cycle_model,
                    ))
                    .child(control_pill("effort", effort, cx, |view| {
                        view.cycle_effort();
                    }))
                    .child(div().flex_1())
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
                        .id("interaction-title")
                        .role(Role::Heading)
                        .aria_level(2)
                        .aria_label(interaction.title.clone())
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
                                .id("gcabb")
                                .accessibility_id("gcabb")
                                .role(Role::Application)
                                .aria_label("GCABB")
                                .track_focus(&self.composer.focus_handle(cx))
                                .on_action(cx.listener(|_, _: &FocusNext, window, cx| {
                                    window.focus_next(cx);
                                }))
                                .on_action(cx.listener(|_, _: &FocusPrevious, window, cx| {
                                    window.focus_prev(cx);
                                }))
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (provider_status, provider_color) = self.provider_status();
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
            .relative()
            .flex()
            .size_full()
            .bg(rgb(BACKGROUND))
            .text_color(rgb(PRIMARY))
            .child(self.sidebar(cx))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .child(
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
                    .child(self.transcript())
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
                    .child(self.composer(cx)),
            )
            .when_some(self.interaction_dialog(cx), |root, dialog| {
                root.child(dialog)
            })
    }
}

fn control_pill(
    id: &'static str,
    value: String,
    cx: &mut Context<SessionMvpView>,
    action: impl Fn(&mut SessionMvpView) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .accessibility_id(id)
        .role(Role::Button)
        .aria_label(format!("{id}: {value}"))
        .focusable()
        .tab_stop(true)
        .px_3()
        .py_1()
        .rounded_md()
        .bg(rgb(ELEVATED))
        .text_xs()
        .text_color(rgb(MUTED))
        .child(value)
        .hover(|style| style.text_color(rgb(PRIMARY)).cursor_pointer())
        .on_click(cx.listener(move |view, _, _, cx| {
            action(view);
            cx.notify();
        }))
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
