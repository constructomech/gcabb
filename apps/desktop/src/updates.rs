//! The in-app update prompt.
//!
//! Update work runs on its own thread with its own Tokio runtime rather than on
//! the session service. An update check is unrelated to session state, must not
//! be delayed behind a long-running agent command, and must keep working when
//! the provider has failed to start — which is exactly when a user is most
//! likely to want a newer build.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::thread;
#[cfg(windows)]
use std::{ffi::OsStr, fs, io::Write as _, process::Stdio};

use updater::install::{InstallLayout, StagedUpdate};
use updater::settings::UpdateSettings;
use updater::source::{GitHubReleaseSource, ProgressCallback, ReqwestClient};
use updater::verify::TrustStore;
use updater::version::BuildStamp;
use updater::{AvailableUpdate, UpdateStatus, Updater};

/// Repository that releases are published to.
const RELEASE_REPOSITORY: &str = "constructomech/gcabb";

/// Overrides the release discovery endpoint.
///
/// Redirecting discovery is safe because discovery confers no trust: a manifest
/// from any endpoint still has to carry a valid signature from the key compiled
/// into this build. That separation is what makes the update loop testable
/// against a local stub feed without weakening it.
const API_BASE_ENV: &str = "GCABB_UPDATE_API_BASE";

/// Builds an updater for the running installation.
fn build_updater(build: &BuildStamp, data_dir: &Path) -> Result<Updater, String> {
    let http = Arc::new(ReqwestClient::new().map_err(|error| error.to_string())?);
    let layout = InstallLayout::for_running_executable().map_err(|error| error.to_string())?;

    let mut source = GitHubReleaseSource::new(Box::new(Arc::clone(&http)), RELEASE_REPOSITORY);
    if let Ok(base) = std::env::var(API_BASE_ENV) {
        tracing::warn!(base, "update discovery is pointed at an override endpoint");
        source = source.with_api_base(base);
    }

    Ok(Updater::new(
        build.clone(),
        TrustStore::embedded(),
        layout,
        Arc::new(source),
        http,
        UpdateSettings::load(data_dir),
    ))
}

/// Exit code reported when a headless run found nothing to do.
pub const EXIT_NOTHING_TO_DO: i32 = 2;

/// Runs the whole update loop without a window and reports what happened.
///
/// This exists so the loop can be exercised on each target platform in CI. The
/// swap semantics that differ between platforms — replacing an executable while
/// it is running — cannot be verified by a unit test, and driving the GUI on
/// three operating systems to check them is not practical.
///
/// Returns a process exit code: 0 applied, 1 failed, 2 nothing to do.
pub fn run_headless(build: &BuildStamp, data_dir: &Path, apply_update: bool) -> i32 {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("could not start the update runtime: {error}");
            return 1;
        }
    };

    let updater = match build_updater(build, data_dir) {
        Ok(updater) => updater,
        Err(error) => {
            eprintln!("{error}");
            return 1;
        }
    };

    // An explicit request, so the automatic-check preference does not apply.
    let status = match runtime.block_on(updater.check(false)) {
        Ok(status) => status,
        Err(error) => {
            eprintln!("update check failed: {error}");
            return 1;
        }
    };

    let available = match status {
        UpdateStatus::Available(available) => *available,
        UpdateStatus::UpToDate => {
            println!("up to date at {}", build.version);
            return EXIT_NOTHING_TO_DO;
        }
        UpdateStatus::Deferred(version) => {
            println!("{version} is available but was deferred");
            return EXIT_NOTHING_TO_DO;
        }
        UpdateStatus::Disabled(reason) => {
            println!("updates unavailable: {}", reason.message());
            return EXIT_NOTHING_TO_DO;
        }
        UpdateStatus::Blocked(reason) => {
            println!("update not applicable: {reason}");
            return EXIT_NOTHING_TO_DO;
        }
    };

    println!(
        "update available: {} -> {}",
        build.version, available.manifest.version
    );
    if !apply_update {
        return 0;
    }

    // Reported in coarse steps: a byte-level log of a quarter-gigabyte
    // download would bury everything else in a CI log.
    let last_decile = std::sync::Mutex::new(u64::MAX);
    let progress: ProgressCallback = Arc::new(move |received, total| {
        let Some(total) = total.filter(|total| *total > 0) else {
            return;
        };
        let decile = received.saturating_mul(10) / total;
        let mut last = match last_decile.lock() {
            Ok(last) => last,
            Err(poisoned) => poisoned.into_inner(),
        };
        if decile != *last {
            *last = decile;
            eprintln!("downloading… {}%", decile * 10);
        }
    });
    let staged = match runtime.block_on(updater.stage(&available, progress)) {
        Ok(staged) => staged,
        Err(error) => {
            eprintln!("staging failed: {error}");
            return 1;
        }
    };
    #[cfg(windows)]
    if let Err(error) = schedule_windows_apply(updater.layout(), &staged, false) {
        eprintln!("could not schedule the update: {error}");
        return 1;
    }
    #[cfg(not(windows))]
    if let Err(error) = updater.apply(&staged) {
        eprintln!("applying failed: {error}");
        return 1;
    }

    #[cfg(windows)]
    println!(
        "scheduled {} for application on exit",
        available.manifest.version
    );
    #[cfg(not(windows))]
    println!("applied {}", available.manifest.version);
    0
}

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
    check_pending: Arc<AtomicBool>,
}

impl UpdateService {
    /// Starts the update worker for the running build.
    #[must_use]
    pub fn start(build: BuildStamp, data_dir: PathBuf) -> Self {
        let (requests, request_rx) = channel();
        let (event_tx, events) = channel();
        let check_pending = Arc::new(AtomicBool::new(false));
        let worker_check_pending = Arc::clone(&check_pending);

        let spawned = thread::Builder::new()
            .name("gcabb-updates".to_owned())
            .spawn(move || {
                run_worker(
                    &build,
                    &data_dir,
                    &request_rx,
                    &event_tx,
                    &worker_check_pending,
                );
            });
        if let Err(error) = spawned {
            tracing::error!(%error, "could not start the update worker");
        }

        Self {
            requests,
            events,
            check_pending,
        }
    }

    /// Asks the worker to do something, ignoring a worker that has stopped.
    pub fn request(&self, request: UpdateRequest) {
        let is_check = matches!(&request, UpdateRequest::Check { .. });
        if is_check && self.check_pending.swap(true, Ordering::AcqRel) {
            return;
        }
        if self.requests.send(request).is_err() {
            if is_check {
                self.check_pending.store(false, Ordering::Release);
            }
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
    check_pending: &AtomicBool,
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

    // Shared with the headless path so there is only one way an updater is
    // built, and the tested path is the shipped one.
    let mut updater = match build_updater(build, data_dir) {
        Ok(updater) => updater,
        Err(error) => {
            let _ = events.send(UpdateEvent::Unavailable(error));
            return;
        }
    };

    // The offered update is held here so Install does not have to check again
    // and risk acting on a different release than the one the user was shown.
    let mut offered: Option<AvailableUpdate> = None;

    while let Ok(request) = requests.recv() {
        match request {
            UpdateRequest::Check { automatic } => {
                if !automatic {
                    let _ = events.send(UpdateEvent::Checking);
                }
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
                        if automatic {
                            tracing::warn!(%error, "automatic update check failed");
                        } else {
                            let _ = events.send(UpdateEvent::Failed(error.to_string()));
                        }
                    }
                }
                check_pending.store(false, Ordering::Release);
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
    #[cfg(not(windows))]
    updater.apply(&staged).map_err(|error| error.to_string())?;
    #[cfg(windows)]
    drop(staged);
    Ok(())
}

/// Relaunches the installed executable and asks the app to quit.
///
/// On Windows a copied helper waits for this process to exit before swapping the
/// locked installation and starting the new build.
pub fn restart_into_updated_build(version: &str) -> Result<(), String> {
    #[cfg(windows)]
    {
        let layout = InstallLayout::for_running_executable().map_err(|error| error.to_string())?;
        let staged = StagedUpdate {
            version: version.to_owned(),
            root: layout.staging_root.join(version),
        };
        schedule_windows_apply(&layout, &staged, true)
    }
    #[cfg(not(windows))]
    {
        let _ = version;
        let current = std::env::current_exe()
            .map_err(|error| format!("could not locate the installed executable: {error}"))?;
        // An update can move the executable within the installation — a macOS
        // build installed before bundling now lives inside GCABB.app — so the
        // relaunch prefers where the applied update actually put it.
        let exe = InstallLayout::for_running_executable()
            .map(|layout| layout.executable_path())
            .ok()
            .filter(|path| path.is_file())
            .unwrap_or(current);
        std::process::Command::new(&exe)
            .spawn()
            .map_err(|error| format!("could not start {}: {error}", exe.display()))?;
        Ok(())
    }
}

#[cfg(windows)]
fn schedule_windows_apply(
    layout: &InstallLayout,
    staged: &StagedUpdate,
    launch: bool,
) -> Result<(), String> {
    let current = std::env::current_exe()
        .map_err(|error| format!("could not locate the installed executable: {error}"))?;
    let helper = layout.update_helper_path();
    if helper.exists() {
        fs::remove_file(&helper)
            .map_err(|error| format!("could not replace {}: {error}", helper.display()))?;
    }
    fs::copy(&current, &helper)
        .map_err(|error| format!("could not create {}: {error}", helper.display()))?;

    let mut child = std::process::Command::new(&helper)
        .arg("--finish-update")
        .arg(&layout.install_dir)
        .arg(&staged.root)
        .arg(&staged.version)
        .arg(if launch { "--launch" } else { "--no-launch" })
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not start {}: {error}", helper.display()))?;
    let parent_lifetime = child
        .stdin
        .take()
        .ok_or_else(|| "the update helper did not open its wait pipe".to_owned())?;

    // The helper blocks on this pipe. Leaking the write end deliberately keeps
    // it open until process teardown, which is the exact signal that the locked
    // installation can be moved safely.
    std::mem::forget(parent_lifetime);
    Ok(())
}

/// Handles the private invocation used by the copied Windows update helper.
///
/// Returns `None` for an ordinary application invocation.
#[cfg(windows)]
pub fn run_update_helper_if_requested() -> Option<i32> {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(OsStr::new("--finish-update")) {
        return None;
    }

    let Some(install_dir) = args.next().map(PathBuf::from) else {
        eprintln!("update helper is missing the installation directory");
        return Some(1);
    };
    let Some(staged_root) = args.next().map(PathBuf::from) else {
        eprintln!("update helper is missing the staged update directory");
        return Some(1);
    };
    let Some(version) = args.next().and_then(|value| value.into_string().ok()) else {
        eprintln!("update helper is missing the update version");
        return Some(1);
    };
    let launch = match args.next().as_deref() {
        Some(value) if value == OsStr::new("--launch") => true,
        Some(value) if value == OsStr::new("--no-launch") => false,
        _ => {
            eprintln!("update helper is missing the launch mode");
            return Some(1);
        }
    };

    let mut input = std::io::stdin().lock();
    if let Err(error) = std::io::copy(&mut input, &mut std::io::sink()) {
        eprintln!("update helper could not wait for the application to exit: {error}");
        return Some(1);
    }

    let layout = InstallLayout::for_install_dir(install_dir);
    let staged = StagedUpdate {
        version: version.clone(),
        root: staged_root,
    };
    if let Err(error) = updater::install::apply(&layout, &staged) {
        eprintln!("applying failed: {error}");
        if launch {
            let executable = layout.executable_path();
            if let Err(restart_error) = std::process::Command::new(&executable).spawn() {
                eprintln!(
                    "could not restart {} after the update failed: {restart_error}",
                    executable.display()
                );
            }
        }
        return Some(1);
    }

    if launch {
        let executable = layout.executable_path();
        if let Err(error) = std::process::Command::new(&executable).spawn() {
            eprintln!("could not start {}: {error}", executable.display());
            return Some(1);
        }
    }
    let _ = writeln!(std::io::stdout(), "applied {version}");
    Some(0)
}

#[cfg(not(windows))]
pub const fn run_update_helper_if_requested() -> Option<i32> {
    None
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
