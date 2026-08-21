//! Scheduled automation execution.
//!
//! Automations run headless: there is no window, no transcript on screen, and
//! nobody to answer a prompt. Each run therefore creates an ephemeral session,
//! drives it to completion, records the outcome, and deletes the session again.
//! The service worker owns the schedule tick; everything it needs to run one
//! automation is bundled into [`AutomationContext`].

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use app_model::{
    Automation, AutomationRun, AutomationRunStatus, SessionKind, SessionStatus, TitleSource,
    TranscriptRole, TranscriptState, next_automation_occurrence, parse_automation_condition,
};
use chrono::{DateTime, SecondsFormat, Utc};
use session_manager::{CreateSessionRequest, SessionHandle, SessionManager};
use storage::Storage;

use crate::{ServiceUpdate, repository_root};

/// How many runs the history view keeps.
const RUN_HISTORY_LIMIT: u32 = 100;
/// How many runs are inspected when recovering from an unclean shutdown.
const RUN_RECOVERY_LIMIT: u32 = 1_000;
/// Upper bound on a single automation run, so a wedged session cannot hold its
/// automation's "already running" slot forever.
const RUN_TIMEOUT: Duration = Duration::from_hours(1);

/// Everything a scheduled automation needs from the service worker.
///
/// Bundling these together keeps the run functions to a couple of arguments
/// each; passing them individually is what previously required
/// `#[allow(clippy::too_many_arguments)]`.
#[derive(Clone)]
pub struct AutomationContext {
    manager: Arc<SessionManager>,
    storage: Arc<Storage>,
    updates: Sender<ServiceUpdate>,
    /// Working directory for automations that target no particular project.
    fallback_workspace: PathBuf,
    /// Automations with a run in flight, so a slow run is never started twice.
    running: Arc<Mutex<HashSet<String>>>,
}

/// How a run ended, kept together so finishing a run stays a two-argument call.
struct RunOutcome {
    status: AutomationRunStatus,
    condition_result: Option<bool>,
    output: Option<String>,
    error: Option<String>,
}

impl RunOutcome {
    fn failed(error: String) -> Self {
        Self {
            status: AutomationRunStatus::Failed,
            condition_result: None,
            output: None,
            error: Some(error),
        }
    }
}

impl AutomationContext {
    pub fn new(
        manager: Arc<SessionManager>,
        storage: Arc<Storage>,
        updates: Sender<ServiceUpdate>,
        fallback_workspace: PathBuf,
    ) -> Self {
        Self {
            manager,
            storage,
            updates,
            fallback_workspace,
            running: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Start every automation whose next run time has passed.
    pub fn dispatch_due(&self, runtime: &tokio::runtime::Runtime) {
        let now = Utc::now();
        let now_text = date_time(now);
        let automations = match self.storage.due_automations(&now_text) {
            Ok(automations) => automations,
            Err(error) => {
                self.report(format!("Failed to check automations: {error}"));
                return;
            }
        };
        if automations.is_empty() {
            return;
        }

        for mut automation in automations {
            if self.is_running(&automation.id) {
                continue;
            }
            let scheduled_for = automation
                .next_run_at
                .clone()
                .unwrap_or_else(|| now_text.clone());
            automation.last_run_at = Some(now_text.clone());
            // Advance the schedule before running, so a crash mid-run cannot
            // leave the automation pinned to a time that keeps re-firing.
            automation.next_run_at =
                next_automation_occurrence(&automation.schedule, &scheduled_for, now)
                    .map(date_time);
            if automation.next_run_at.is_none() {
                automation.enabled = false;
            }
            automation.updated_at.clone_from(&now_text);
            if let Err(error) = self.storage.upsert_automation(&automation) {
                self.report(format!(
                    "Failed to advance automation \"{}\": {error}",
                    automation.name
                ));
                continue;
            }
            self.spawn_run(runtime, automation, scheduled_for);
        }
        self.broadcast_automations();
    }

    /// Start one automation immediately, reporting when it is already running.
    pub fn run_now(&self, runtime: &tokio::runtime::Runtime, automation_id: &str) {
        let automation = self
            .storage
            .list_automations()
            .unwrap_or_default()
            .into_iter()
            .find(|automation| automation.id == automation_id);
        let Some(automation) = automation else {
            self.report("Automation no longer exists.".to_owned());
            return;
        };
        if !self.spawn_run(runtime, automation, timestamp()) {
            self.report("That automation is already running.".to_owned());
        }
    }

    /// Create or update an automation, recomputing when it next fires.
    pub fn save(&self, mut automation: Automation) {
        let now = timestamp();
        if automation.created_at.is_empty() {
            automation.created_at.clone_from(&now);
        }
        automation.updated_at = now;
        automation.next_run_at = if automation.enabled {
            automation.schedule.next_after(Utc::now()).map(date_time)
        } else {
            None
        };
        if let Err(error) = self.storage.upsert_automation(&automation) {
            self.report(error.to_string());
        } else {
            self.broadcast_automations();
        }
    }

    pub fn delete(&self, automation_id: &str) {
        if let Err(error) = self.storage.delete_automation(automation_id) {
            self.report(error.to_string());
        } else {
            self.broadcast_automations();
        }
    }

    /// Claim the automation's run slot and drive it on the async runtime.
    ///
    /// Returns whether the run started; a second concurrent run is refused
    /// rather than queued.
    fn spawn_run(
        &self,
        runtime: &tokio::runtime::Runtime,
        automation: Automation,
        scheduled_for: String,
    ) -> bool {
        let Ok(mut running) = self.running.lock() else {
            return false;
        };
        if !running.insert(automation.id.clone()) {
            return false;
        }
        drop(running);
        let context = self.clone();
        let automation_id = automation.id.clone();
        runtime.spawn(async move {
            context.execute(automation, scheduled_for).await;
            if let Ok(mut running) = context.running.lock() {
                running.remove(&automation_id);
            }
        });
        true
    }

    fn is_running(&self, automation_id: &str) -> bool {
        run_in_flight(&self.running, automation_id)
    }

    /// Run one automation end to end in a throwaway session.
    async fn execute(&self, automation: Automation, scheduled_for: String) {
        let mut run = AutomationRun {
            id: uuid::Uuid::new_v4().to_string(),
            automation_id: automation.id.clone(),
            automation_name: automation.name.clone(),
            scheduled_for,
            started_at: timestamp(),
            finished_at: None,
            status: AutomationRunStatus::Running,
            condition_result: None,
            output: None,
            error: None,
            session_id: None,
        };
        self.persist_run(&run);

        let (working_directory, repository, kind) = automation.project_path.as_ref().map_or_else(
            || (self.fallback_workspace.clone(), None, SessionKind::Chat),
            |path| {
                let path = PathBuf::from(path);
                let repository = repository_root(&path).to_string_lossy().into_owned();
                (path, Some(repository), SessionKind::Project)
            },
        );
        if !working_directory.is_dir() {
            let message = format!(
                "Automation workspace is unavailable: {}",
                working_directory.display()
            );
            self.finish_run(&mut run, RunOutcome::failed(message));
            return;
        }

        let handle = match self
            .manager
            .create_session(CreateSessionRequest {
                project_path: working_directory,
                title: format!("Automation: {}", automation.name),
                title_source: TitleSource::Manual,
                model: automation.model.clone(),
                mode: Some(automation.mode.clone()),
                agent: automation.agent.clone(),
                reasoning_effort: automation.reasoning_effort.clone(),
                context_tier: automation.context_tier.clone(),
                base_ref: None,
                repository_root: repository,
                kind,
                // Headless: a permission prompt would park the session in
                // `Waiting` with no window in which to answer it.
                unattended: true,
            })
            .await
        {
            Ok(handle) => handle,
            Err(error) => {
                self.finish_run(&mut run, RunOutcome::failed(error.to_string()));
                return;
            }
        };
        let session_id = handle.id().to_owned();
        run.session_id = Some(session_id.clone());
        self.persist_run(&run);

        let execution = self.drive(&handle, &automation, &mut run).await;

        if let Err(error) = self.manager.close_session(&session_id).await {
            tracing::warn!(%error, %session_id, "failed to close automation session");
        }
        if let Err(error) = self.storage.delete_session(&session_id) {
            tracing::warn!(%error, %session_id, "failed to remove ephemeral automation session");
        }

        let outcome = match execution {
            Ok((status, condition_result, output)) => RunOutcome {
                status,
                condition_result,
                output,
                error: None,
            },
            Err(error) => RunOutcome {
                status: AutomationRunStatus::Failed,
                condition_result: run.condition_result,
                output: None,
                error: Some(error),
            },
        };
        self.finish_run(&mut run, outcome);
    }

    /// Evaluate the optional condition, then perform the action.
    async fn drive(
        &self,
        handle: &SessionHandle,
        automation: &Automation,
        run: &mut AutomationRun,
    ) -> Result<(AutomationRunStatus, Option<bool>, Option<String>), String> {
        if let Some(condition) = automation
            .condition
            .as_deref()
            .map(str::trim)
            .filter(|condition| !condition.is_empty())
        {
            let condition_prompt = format!(
                "Evaluate this saved automation condition at the current moment:\n\n\
                 {condition}\n\n\
                 Use available read-only inspection tools when needed. Do not perform the \
                 automation action and do not change files or external state. Your final response \
                 must be exactly one word: true or false."
            );
            let response = send_prompt(handle, condition_prompt).await?;
            let result = parse_automation_condition(&response)?;
            run.condition_result = Some(result);
            self.persist_run(run);
            if !result {
                return Ok((AutomationRunStatus::Skipped, Some(false), Some(response)));
            }
        }

        let action_prompt = format!(
            "Run the saved automation \"{}\" now.\n\nInstructions:\n{}",
            automation.name, automation.instructions
        );
        let output = send_prompt(handle, action_prompt).await?;
        Ok((
            AutomationRunStatus::Succeeded,
            automation.condition.as_ref().map(|_| true),
            Some(output),
        ))
    }

    fn finish_run(&self, run: &mut AutomationRun, outcome: RunOutcome) {
        run.finished_at = Some(timestamp());
        run.status = outcome.status;
        run.condition_result = outcome.condition_result;
        run.output = outcome.output;
        run.error = outcome.error;
        self.persist_run(run);
    }

    fn persist_run(&self, run: &AutomationRun) {
        if let Err(error) = self.storage.upsert_automation_run(run) {
            self.report(format!("Failed to save automation run: {error}"));
            return;
        }
        let _ = self.updates.send(ServiceUpdate::AutomationRunsChanged(
            self.storage
                .list_automation_runs(RUN_HISTORY_LIMIT)
                .unwrap_or_default(),
        ));
    }

    fn broadcast_automations(&self) {
        let _ = self.updates.send(ServiceUpdate::AutomationsChanged(
            self.storage.list_automations().unwrap_or_default(),
        ));
    }

    fn report(&self, message: String) {
        let _ = self.updates.send(ServiceUpdate::ActionFailed(message));
    }
}

/// Whether a run is already in flight for `automation_id`.
///
/// A poisoned lock reports `true` so a run is skipped rather than started
/// against unknown state.
fn run_in_flight(running: &Mutex<HashSet<String>>, automation_id: &str) -> bool {
    running
        .lock()
        .map_or(true, |running| running.contains(automation_id))
}

/// Now, in the timestamp format automations persist.
pub fn timestamp() -> String {
    date_time(Utc::now())
}

/// Render an instant in the timestamp format automations persist.
pub fn date_time(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Fail runs that were still in flight when GCABB last closed.
///
/// Their sessions are gone with the process, so the rows would otherwise stay
/// "running" forever. The ephemeral session each one created is removed too.
pub fn recover_interrupted_runs(storage: &Storage) {
    let Ok(runs) = storage.list_automation_runs(RUN_RECOVERY_LIMIT) else {
        return;
    };
    for mut run in runs
        .into_iter()
        .filter(|run| run.status == AutomationRunStatus::Running)
    {
        if let Some(session_id) = run.session_id.as_deref()
            && let Err(error) = storage.delete_session(session_id)
        {
            tracing::warn!(%error, %session_id, "failed to remove interrupted automation session");
        }
        run.status = AutomationRunStatus::Failed;
        run.finished_at = Some(timestamp());
        run.error = Some("GCABB closed before this automation run completed.".to_owned());
        if let Err(error) = storage.upsert_automation_run(&run) {
            tracing::warn!(%error, run_id = %run.id, "failed to mark automation run interrupted");
        }
    }
}

/// Send one prompt and wait for the turn it starts to finish.
///
/// Waiting on `last_sequence` moving past the pre-send value keeps a stale
/// idle snapshot from being mistaken for this turn's completion.
async fn send_prompt(handle: &SessionHandle, prompt: String) -> Result<String, String> {
    let before_sequence = handle.snapshot().last_sequence;
    let mut snapshots = handle.subscribe();
    handle
        .send(prompt)
        .await
        .map_err(|error| error.to_string())?;
    let snapshot = tokio::time::timeout(
        RUN_TIMEOUT,
        snapshots.wait_for(|snapshot| {
            snapshot.last_sequence > before_sequence
                && matches!(
                    snapshot.status,
                    SessionStatus::Idle | SessionStatus::Failed | SessionStatus::Disconnected
                )
        }),
    )
    .await
    .map_err(|_| "Automation run timed out after one hour.".to_owned())?
    .map_err(|_| "Automation session closed before it completed.".to_owned())?
    .clone();
    if matches!(
        snapshot.status,
        SessionStatus::Failed | SessionStatus::Disconnected
    ) {
        return Err(snapshot
            .last_error
            .clone()
            .unwrap_or_else(|| "Automation session failed.".to_owned()));
    }
    snapshot
        .transcript
        .iter()
        .rev()
        .find(|message| {
            message.role == TranscriptRole::Assistant
                && message.state == TranscriptState::Complete
                && message.sequence > before_sequence
        })
        .map(|message| message.content.clone())
        .filter(|content| !content.trim().is_empty())
        .ok_or_else(|| "Automation completed without an assistant response.".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use app_model::{SessionMetadata, TitleSource};

    #[test]
    fn running_automation_guard_detects_and_releases_ids() {
        let running = Mutex::new(HashSet::new());
        assert!(!run_in_flight(&running, "automation-1"));
        running.lock().unwrap().insert("automation-1".to_owned());
        assert!(run_in_flight(&running, "automation-1"));
        running.lock().unwrap().remove("automation-1");
        assert!(!run_in_flight(&running, "automation-1"));
    }

    #[test]
    fn interrupted_automation_runs_are_failed_and_ephemeral_sessions_removed() {
        let storage = Storage::open_in_memory().unwrap();
        storage
            .upsert_session(&SessionMetadata {
                id: "automation-session".to_owned(),
                sdk_session_id: "automation-sdk-session".to_owned(),
                project_path: std::env::temp_dir().to_string_lossy().into_owned(),
                repository_root: None,
                title: "Automation: maintenance".to_owned(),
                title_source: TitleSource::Manual,
                kind: SessionKind::Chat,
                model: None,
                mode: Some("autopilot".to_owned()),
                base_ref: None,
                created_at: "2026-08-14T10:00:00Z".to_owned(),
                updated_at: "2026-08-14T10:00:00Z".to_owned(),
            })
            .unwrap();
        storage
            .upsert_automation_run(&AutomationRun {
                id: "run-1".to_owned(),
                automation_id: "automation-1".to_owned(),
                automation_name: "Maintenance".to_owned(),
                scheduled_for: "2026-08-14T10:00:00Z".to_owned(),
                started_at: "2026-08-14T10:00:01Z".to_owned(),
                finished_at: None,
                status: AutomationRunStatus::Running,
                condition_result: None,
                output: None,
                error: None,
                session_id: Some("automation-session".to_owned()),
            })
            .unwrap();

        recover_interrupted_runs(&storage);

        assert!(storage.list_sessions().unwrap().is_empty());
        let runs = storage.list_automation_runs(10).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, AutomationRunStatus::Failed);
        assert!(runs[0].finished_at.is_some());
        assert!(
            runs[0]
                .error
                .as_deref()
                .is_some_and(|error| error.contains("closed before"))
        );
    }
}
