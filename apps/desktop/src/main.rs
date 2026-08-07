use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::thread;
use std::time::Duration;

use copilot_provider::{CopilotProvider, ProviderCompatibility};
use diagnostics::{TracingDiagnostics, init_tracing};
use gpui::prelude::FluentBuilder;
use gpui::{
    App, AppContext, Application, Bounds, Context, IntoElement, ParentElement, Render, Styled,
    Timer, Window, WindowBounds, WindowOptions, div, px, rgb, size,
};
use session_manager::{RestoreFailure, SessionHandle, SessionManager};
use storage::Storage;
use tokio::sync::watch;

const BACKGROUND: u32 = 0x0011_1318;
const PANEL: u32 = 0x001a_1d24;
const BORDER: u32 = 0x0030_3642;
const PRIMARY: u32 = 0x00e8_ecf2;
const MUTED: u32 = 0x008d_96a8;
const GREEN: u32 = 0x0063_d392;
const BLUE: u32 = 0x006b_a6ff;
const AMBER: u32 = 0x00e6_b566;
const RED: u32 = 0x00ee_6a70;

enum ServiceUpdate {
    Ready {
        compatibility: ProviderCompatibility,
        restored: Vec<SessionHandle>,
        failures: Vec<RestoreFailure>,
    },
    Failed(String),
}

enum ServiceCommand {
    Stop,
}

struct FoundationService {
    updates: Receiver<ServiceUpdate>,
    commands: Sender<ServiceCommand>,
    stopped: Receiver<()>,
}

impl FoundationService {
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
                let provider = Arc::new(CopilotProvider::new(project_root, diagnostics.clone()));
                let manager = Arc::new(SessionManager::new(provider, storage, diagnostics));

                match runtime.block_on(manager.start()) {
                    Ok((compatibility, report)) => {
                        let _ = update_tx.send(ServiceUpdate::Ready {
                            compatibility,
                            restored: report.restored,
                            failures: report.failed,
                        });
                    }
                    Err(error) => {
                        let _ = update_tx.send(ServiceUpdate::Failed(format!(
                            "Copilot provider startup failed: {error}"
                        )));
                    }
                }

                if matches!(command_rx.recv(), Ok(ServiceCommand::Stop)) {
                    let _ = runtime.block_on(manager.stop());
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

struct SessionProjection {
    receiver: watch::Receiver<Arc<app_model::SessionSnapshot>>,
    snapshot: Arc<app_model::SessionSnapshot>,
}

enum StartupState {
    Starting,
    Ready(ProviderCompatibility),
    Failed(String),
}

struct FoundationView {
    startup: StartupState,
    sessions: Vec<SessionProjection>,
    restore_failures: Vec<RestoreFailure>,
    updates: Receiver<ServiceUpdate>,
    base: String,
    diff_summary: String,
    _poll_task: gpui::Task<()>,
}

impl FoundationView {
    fn new(
        service: FoundationService,
        base: String,
        diff_summary: String,
        cx: &mut Context<Self>,
    ) -> Self {
        let commands = service.commands;
        let stopped = service.stopped;
        cx.on_app_quit(move |_, _| {
            let _ = commands.send(ServiceCommand::Stop);
            let _ = stopped.recv_timeout(Duration::from_secs(5));
            async {}
        })
        .detach();

        let poll_task = cx.spawn(async move |view, cx| {
            loop {
                Timer::after(Duration::from_millis(33)).await;
                if view
                    .update(cx, |view, cx| {
                        view.apply_service_updates();
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
            sessions: Vec::new(),
            restore_failures: Vec::new(),
            updates: service.updates,
            base,
            diff_summary,
            _poll_task: poll_task,
        }
    }

    fn apply_service_updates(&mut self) {
        loop {
            match self.updates.try_recv() {
                Ok(ServiceUpdate::Ready {
                    compatibility,
                    restored,
                    failures,
                }) => {
                    self.startup = StartupState::Ready(compatibility);
                    self.sessions = restored
                        .into_iter()
                        .map(|handle| {
                            let receiver = handle.subscribe();
                            let snapshot = receiver.borrow().clone();
                            SessionProjection { receiver, snapshot }
                        })
                        .collect();
                    self.restore_failures = failures;
                }
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

    fn panel(title: &str, accent: u32) -> gpui::Div {
        div()
            .flex()
            .flex_col()
            .gap_2()
            .p_4()
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(BORDER))
            .rounded_md()
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(accent))
                    .child(title.to_owned()),
            )
    }

    fn provider_status(&self) -> (String, u32) {
        match &self.startup {
            StartupState::Starting => ("Starting Copilot provider...".to_owned(), AMBER),
            StartupState::Ready(compatibility) => (
                format!(
                    "SDK {} · protocol {} · pid {}",
                    compatibility.sdk_crate_version,
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

    fn sessions_panel(&self) -> gpui::Div {
        let sessions = self.sessions.iter().map(|session| {
            div()
                .flex()
                .flex_col()
                .gap_1()
                .p_2()
                .border_b_1()
                .border_color(rgb(BORDER))
                .child(
                    div()
                        .text_color(rgb(PRIMARY))
                        .child(session.snapshot.metadata.title.clone()),
                )
                .child(div().text_sm().text_color(rgb(MUTED)).child(format!(
                    "{:?} · {} events",
                    session.snapshot.status,
                    session.snapshot.activities.len()
                )))
                .when_some(session.snapshot.last_error.clone(), |card, error| {
                    card.child(div().text_sm().text_color(rgb(RED)).child(error))
                })
        });
        let failures = self.restore_failures.iter().map(|failure| {
            div()
                .text_sm()
                .text_color(rgb(RED))
                .child(format!("{}: {}", failure.app_session_id, failure.error))
        });

        Self::panel("RECOVERED SESSIONS", BLUE)
            .w(px(300.0))
            .children(sessions)
            .children(failures)
            .when(self.sessions.is_empty(), |panel| {
                panel.child(
                    div()
                        .text_sm()
                        .text_color(rgb(MUTED))
                        .child("No persisted sessions"),
                )
            })
    }

    fn activity_panel(&self) -> gpui::Div {
        let selected = self.sessions.first();
        let activities = selected.map_or(&[][..], |session| session.snapshot.activities.as_slice());
        let timeline = activities
            .iter()
            .skip(activities.len().saturating_sub(100))
            .map(|event| {
                div()
                    .py_1()
                    .text_sm()
                    .text_color(rgb(PRIMARY))
                    .child(format!(
                        "{:>5}  {}  {}",
                        event.sequence, event.source_type, event.summary
                    ))
            });

        Self::panel(
            &format!("ACTIVITY PROJECTION · {} EVENTS", activities.len()),
            GREEN,
        )
        .flex_1()
        .overflow_hidden()
        .children(timeline)
        .when(selected.is_none(), |panel| {
            panel.child(
                div()
                    .text_color(rgb(MUTED))
                    .child("Select or create a session in Phase 2"),
            )
        })
    }

    fn changes_panel(&self) -> gpui::Div {
        Self::panel("SELECTABLE-BASE GIT SNAPSHOT", AMBER)
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(PRIMARY))
                    .child(format!("base: {}", self.base)),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(MUTED))
                    .child(self.diff_summary.clone()),
            )
    }
}

impl Render for FoundationView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let (provider_status, provider_color) = self.provider_status();

        div()
            .flex()
            .flex_col()
            .size_full()
            .gap_3()
            .p_4()
            .bg(rgb(BACKGROUND))
            .text_color(rgb(PRIMARY))
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_center()
                    .child(div().text_xl().child("GCABB / Application Foundation"))
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(provider_color))
                            .child(provider_status),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .gap_3()
                    .child(self.sessions_panel())
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .gap_3()
                            .child(self.activity_panel())
                            .child(self.changes_panel()),
                    ),
            )
    }
}

fn git_diff_summary(base: &str) -> String {
    let output = Command::new("git")
        .args(["diff", "--shortstat", base, "--"])
        .output();

    match output {
        Ok(output) if output.status.success() => {
            let summary = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            if summary.is_empty() {
                "no changes".to_owned()
            } else {
                summary
            }
        }
        Ok(output) => format!(
            "git diff failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
        Err(error) => format!("git unavailable: {error}"),
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

fn project_root() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| Path::new(".").to_owned())
}

fn main() {
    if let Err(error) = init_tracing("gcabb=info") {
        eprintln!("failed to initialize structured tracing: {error}");
    }
    let base = std::env::args().nth(1).unwrap_or_else(|| "HEAD".to_owned());
    let diff_summary = git_diff_summary(&base);
    let root = project_root();
    let service = match database_path() {
        Ok(path) => FoundationService::start(root, path),
        Err(error) => FoundationService::failed(error),
    };

    Application::new().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1200.0), px(760.0)), cx);
        let base = base.clone();
        let diff_summary = diff_summary.clone();
        let service = service;
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            move |_, cx| cx.new(|cx| FoundationView::new(service, base, diff_summary, cx)),
        )
        .expect("failed to open GPUI foundation window");
        cx.activate(true);
    });
}
