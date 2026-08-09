//! The in-app update prompt.
//!
//! Update work runs on its own thread with its own Tokio runtime rather than on
//! the session service. An update check is unrelated to session state, must not
//! be delayed behind a long-running agent command, and must keep working when
//! the provider has failed to start — which is exactly when a user is most
//! likely to want a newer build.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::thread;

use updater::install::{InstallLayout, StagedUpdate};
use updater::settings::UpdateSettings;
use updater::source::{GitHubReleaseSource, ProgressCallback, ReqwestClient};
use updater::verify::TrustStore;
use updater::version::BuildStamp;
use updater::{AvailableUpdate, UpdateStatus, Updater};

/// Repository that releases are published to.
const RELEASE_REPOSITORY: &str = "constructomech/gcabb";

/// What the user asked the updater to do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UpdateRequest {
    /// Look for an update. `automatic` marks a background check, which honours
    /// the user's preference to disable automatic checking.
    Check { automatic: bool },
    /// Download, verify, and stage the offered update, then apply it.
    Install,
    /// Skip the offered version.
    Defer,
}

/// A change in update state, delivered to the UI thread.
#[derive(Clone, Debug, PartialEq)]
pub enum UpdateEvent {
    Checking,
    UpToDate,
    Available {
        version: String,
        notes: String,
    },
    Progress {
        received: u64,
        total: Option<u64>,
    },
    Installed {
        version: String,
    },
    Dismissed,
    /// Updates cannot proceed, with a reason worth showing.
    Unavailable(String),
    Failed(String),
}

/// What the update banner is showing.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum UpdateUi {
    /// Nothing to say; the banner is hidden.
    #[default]
    Hidden,
    Checking,
    Available {
        version: String,
        notes: String,
    },
    Downloading {
        received: u64,
        total: Option<u64>,
    },
    /// The update is on disk and takes effect on restart.
    ReadyToRestart {
        version: String,
    },
    Failed(String),
}

impl UpdateUi {
    /// Applies an event, returning whether the banner changed.
    pub fn apply(&mut self, event: UpdateEvent) -> bool {
        let next = match event {
            UpdateEvent::Checking => Self::Checking,
            // A silent background check that finds nothing must not leave a
            // banner on screen, so these collapse to hidden.
            UpdateEvent::UpToDate | UpdateEvent::Dismissed => Self::Hidden,
            UpdateEvent::Available { version, notes } => Self::Available { version, notes },
            UpdateEvent::Progress { received, total } => Self::Downloading { received, total },
            UpdateEvent::Installed { version } => Self::ReadyToRestart { version },
            // A build that cannot update is a normal deployment, not an error,
            // so it is logged rather than shown as a failure banner.
            UpdateEvent::Unavailable(reason) => {
                tracing::info!(reason, "updates are unavailable for this build");
                Self::Hidden
            }
            UpdateEvent::Failed(message) => Self::Failed(message),
        };
        let changed = *self != next;
        *self = next;
        changed
    }

    /// Download progress as a percentage, when the total size is known.
    #[must_use]
    pub fn percent(&self) -> Option<u8> {
        match self {
            Self::Downloading {
                received,
                total: Some(total),
            } if *total > 0 => {
                // Integer maths: an artifact large enough to lose precision in
                // an f64 would be a bug elsewhere, and this avoids the cast
                // entirely.
                let percent = received.saturating_mul(100) / total;
                Some(u8::try_from(percent.min(100)).unwrap_or(100))
            }
            _ => None,
        }
    }
}

/// Handle to the background update worker.
pub struct UpdateService {
    requests: Sender<UpdateRequest>,
    events: Receiver<UpdateEvent>,
}

impl UpdateService {
    /// Starts the update worker for the running build.
    #[must_use]
    pub fn start(build: BuildStamp, data_dir: PathBuf) -> Self {
        let (requests, request_rx) = channel();
        let (event_tx, events) = channel();

        let spawned = thread::Builder::new()
            .name("gcabb-updates".to_owned())
            .spawn(move || run_worker(&build, &data_dir, &request_rx, &event_tx));
        if let Err(error) = spawned {
            tracing::error!(%error, "could not start the update worker");
        }

        Self { requests, events }
    }

    /// Asks the worker to do something, ignoring a worker that has stopped.
    pub fn request(&self, request: UpdateRequest) {
        if self.requests.send(request).is_err() {
            tracing::warn!("the update worker is not running");
        }
    }

    /// Drains pending events into the banner, returning whether it changed.
    pub fn drain(&self, ui: &mut UpdateUi) -> bool {
        let mut changed = false;
        loop {
            match self.events.try_recv() {
                Ok(event) => changed |= ui.apply(event),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return changed,
            }
        }
    }
}

/// Builds the updater and services requests until the UI goes away.
fn run_worker(
    build: &BuildStamp,
    data_dir: &Path,
    requests: &Receiver<UpdateRequest>,
    events: &Sender<UpdateEvent>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = events.send(UpdateEvent::Failed(format!(
                "could not start the update runtime: {error}"
            )));
            return;
        }
    };

    let http = match ReqwestClient::new() {
        Ok(client) => Arc::new(client),
        Err(error) => {
            let _ = events.send(UpdateEvent::Failed(error.to_string()));
            return;
        }
    };
    let layout = match InstallLayout::for_running_executable() {
        Ok(layout) => layout,
        Err(error) => {
            let _ = events.send(UpdateEvent::Unavailable(error.to_string()));
            return;
        }
    };
    let source = Arc::new(GitHubReleaseSource::new(
        Box::new(Arc::clone(&http)),
        RELEASE_REPOSITORY,
    ));

    let mut updater = Updater::new(
        build.clone(),
        TrustStore::embedded(),
        layout,
        source,
        http,
        UpdateSettings::load(data_dir),
    );

    // The offered update is held here so Install does not have to check again
    // and risk acting on a different release than the one the user was shown.
    let mut offered: Option<AvailableUpdate> = None;

    while let Ok(request) = requests.recv() {
        match request {
            UpdateRequest::Check { automatic } => {
                let _ = events.send(UpdateEvent::Checking);
                match runtime.block_on(updater.check(automatic)) {
                    Ok(UpdateStatus::Available(available)) => {
                        let _ = events.send(UpdateEvent::Available {
                            version: available.manifest.version.to_string(),
                            notes: available.manifest.notes.clone(),
                        });
                        offered = Some(*available);
                    }
                    Ok(UpdateStatus::UpToDate | UpdateStatus::Deferred(_)) => {
                        let _ = events.send(UpdateEvent::UpToDate);
                    }
                    Ok(UpdateStatus::Disabled(reason)) => {
                        let _ = events.send(UpdateEvent::Unavailable(reason.message().to_owned()));
                    }
                    Ok(UpdateStatus::Blocked(reason)) => {
                        let _ = events.send(UpdateEvent::Unavailable(reason));
                    }
                    Err(error) => {
                        let _ = events.send(UpdateEvent::Failed(error.to_string()));
                    }
                }
            }
            UpdateRequest::Install => {
                let Some(available) = offered.clone() else {
                    continue;
                };
                match install(&runtime, &updater, &available, events) {
                    Ok(()) => {
                        let _ = events.send(UpdateEvent::Installed {
                            version: available.manifest.version.to_string(),
                        });
                        offered = None;
                    }
                    Err(message) => {
                        let _ = events.send(UpdateEvent::Failed(message));
                    }
                }
            }
            UpdateRequest::Defer => {
                if let Some(available) = offered.take() {
                    updater.settings_mut().defer(available.manifest.version);
                    if let Err(error) = updater.settings().save(data_dir) {
                        tracing::warn!(%error, "could not record the deferred version");
                    }
                }
                let _ = events.send(UpdateEvent::Dismissed);
            }
        }
    }
}

/// Downloads, verifies, stages, and applies an update.
fn install(
    runtime: &tokio::runtime::Runtime,
    updater: &Updater,
    available: &AvailableUpdate,
    events: &Sender<UpdateEvent>,
) -> Result<(), String> {
    let reporter = events.clone();
    let progress: ProgressCallback = Arc::new(move |received, total| {
        let _ = reporter.send(UpdateEvent::Progress { received, total });
    });

    let staged: StagedUpdate = runtime
        .block_on(updater.stage(available, progress))
        .map_err(|error| error.to_string())?;
    updater.apply(&staged).map_err(|error| error.to_string())
}

/// Relaunches the installed executable and asks the app to quit.
///
/// The new build is started before the current one exits so a failure to spawn
/// is reported while there is still a window to report it in.
pub fn restart_into_updated_build() -> Result<(), String> {
    let exe = std::env::current_exe()
        .map_err(|error| format!("could not locate the installed executable: {error}"))?;
    std::process::Command::new(&exe)
        .spawn()
        .map_err(|error| format!("could not start {}: {error}", exe.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{UpdateEvent, UpdateUi};

    #[test]
    fn an_offered_update_shows_the_banner() {
        let mut ui = UpdateUi::default();
        assert!(ui.apply(UpdateEvent::Available {
            version: "0.2.0".to_owned(),
            notes: "notes".to_owned(),
        }));
        assert!(matches!(ui, UpdateUi::Available { .. }));
    }

    #[test]
    fn a_background_check_that_finds_nothing_shows_no_banner() {
        let mut ui = UpdateUi::default();
        ui.apply(UpdateEvent::Checking);
        ui.apply(UpdateEvent::UpToDate);
        assert_eq!(ui, UpdateUi::Hidden);
    }

    #[test]
    fn a_build_that_cannot_update_shows_no_banner() {
        let mut ui = UpdateUi::default();
        ui.apply(UpdateEvent::Unavailable("developer build".to_owned()));
        assert_eq!(ui, UpdateUi::Hidden);
    }

    #[test]
    fn deferring_hides_the_banner() {
        let mut ui = UpdateUi::default();
        ui.apply(UpdateEvent::Available {
            version: "0.2.0".to_owned(),
            notes: String::new(),
        });
        ui.apply(UpdateEvent::Dismissed);
        assert_eq!(ui, UpdateUi::Hidden);
    }

    #[test]
    fn a_failure_is_surfaced_rather_than_hidden() {
        let mut ui = UpdateUi::default();
        ui.apply(UpdateEvent::Failed("signature mismatch".to_owned()));
        assert_eq!(ui, UpdateUi::Failed("signature mismatch".to_owned()));
    }

    #[test]
    fn progress_reports_a_percentage_when_the_size_is_known() {
        let mut ui = UpdateUi::default();
        ui.apply(UpdateEvent::Progress {
            received: 50,
            total: Some(200),
        });
        assert_eq!(ui.percent(), Some(25));
    }

    #[test]
    fn progress_without_a_known_size_reports_no_percentage() {
        let mut ui = UpdateUi::default();
        ui.apply(UpdateEvent::Progress {
            received: 50,
            total: None,
        });
        assert_eq!(ui.percent(), None);
    }

    #[test]
    fn an_applied_update_asks_for_a_restart() {
        let mut ui = UpdateUi::default();
        ui.apply(UpdateEvent::Installed {
            version: "0.2.0".to_owned(),
        });
        assert_eq!(
            ui,
            UpdateUi::ReadyToRestart {
                version: "0.2.0".to_owned()
            }
        );
    }

    #[test]
    fn repeating_an_event_reports_no_change() {
        let mut ui = UpdateUi::default();
        assert!(ui.apply(UpdateEvent::Checking));
        assert!(!ui.apply(UpdateEvent::Checking));
    }
}
