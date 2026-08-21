use std::fs::{File, create_dir_all};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use app_model::DomainEvent;
use async_trait::async_trait;
use clap::Parser;
use github_copilot_sdk::handler::{
    AutoModeSwitchHandler, AutoModeSwitchResponse, ElicitationHandler, ExitPlanModeHandler,
    ExitPlanModeResult, UserInputHandler, UserInputResponse,
};
use github_copilot_sdk::hooks::{HookEvent, HookOutput, SessionHooks};
use github_copilot_sdk::rpc::{
    AgentsDiscoverRequest, InstructionsDiscoverRequest, SkillsDiscoverRequest,
};
use github_copilot_sdk::session::Session;
use github_copilot_sdk::tool::ToolHandler;
use github_copilot_sdk::{
    Client, ClientOptions, DeliveryMode, ElicitationRequest, ElicitationResult, MessageOptions,
    RequestId, ResumeSessionConfig, SessionConfig, SessionId, Tool, ToolInvocation, ToolResult,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

#[derive(Clone, Debug, Parser)]
#[command(about = "Record Copilot SDK events for the GCABB Phase 0 feasibility matrix")]
#[allow(clippy::struct_excessive_bools, reason = "CLI flags map to bools")]
struct Args {
    #[arg(long, default_value = ".")]
    cwd: PathBuf,
    #[arg(long, default_value = ".phase0/events.jsonl")]
    output: PathBuf,
    #[arg(long)]
    resume: Option<String>,
    #[arg(long)]
    prompt: Option<String>,
    #[arg(long, default_value_t = 120)]
    timeout_seconds: u64,
    #[arg(long)]
    abort_after_ms: Option<u64>,
    #[arg(long)]
    approve_permissions: bool,
    #[arg(long)]
    fleet_prompt: Option<String>,
    /// Interrupt a running turn with an immediate-delivery send to verify
    /// steering works on the stable `session.send` surface. Consumes quota.
    #[arg(long)]
    steering_probe: bool,
}

#[derive(Clone)]
struct Recorder {
    writer: Arc<Mutex<BufWriter<File>>>,
    started: Instant,
}

#[derive(Serialize)]
struct Record<'a, T> {
    elapsed_ms: u128,
    channel: &'a str,
    payload: T,
}

impl Recorder {
    fn create(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let file =
            File::create(path).with_context(|| format!("failed to create {}", path.display()))?;
        Ok(Self {
            writer: Arc::new(Mutex::new(BufWriter::new(file))),
            started: Instant::now(),
        })
    }

    fn write<T: Serialize>(&self, channel: &str, payload: T) -> Result<()> {
        let record = Record {
            elapsed_ms: self.started.elapsed().as_millis(),
            channel,
            payload,
        };
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| anyhow::anyhow!("event recorder lock poisoned"))?;
        serde_json::to_writer(&mut *writer, &record)?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        Ok(())
    }
}

struct ProbeCallbacks {
    recorder: Recorder,
}

#[derive(Deserialize)]
struct TerminalArgs {
    program: String,
    #[serde(default)]
    args: Vec<String>,
}

struct HostTerminalTool {
    recorder: Recorder,
}

#[async_trait]
impl ToolHandler for HostTerminalTool {
    async fn call(&self, invocation: ToolInvocation) -> github_copilot_sdk::Result<ToolResult> {
        let args: TerminalArgs = serde_json::from_value(invocation.arguments)?;
        let _ = self.recorder.write(
            "terminal.started",
            json!({
                "toolCallId": invocation.tool_call_id,
                "program": args.program,
                "args": args.args,
            }),
        );

        let mut child = tokio::process::Command::new(&args.program)
            .args(&args.args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::other("stdout pipe unavailable"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| std::io::Error::other("stderr pipe unavailable"))?;
        let (tx, mut rx) = mpsc::channel(128);
        tokio::spawn(pump_lines("stdout", stdout, tx.clone()));
        tokio::spawn(pump_lines("stderr", stderr, tx));

        let mut tail = std::collections::VecDeque::with_capacity(200);
        while let Some((stream, line)) = rx.recv().await {
            let _ = self.recorder.write(
                "terminal.output",
                json!({
                    "toolCallId": invocation.tool_call_id,
                    "stream": stream,
                    "line": line,
                }),
            );
            if tail.len() == 200 {
                tail.pop_front();
            }
            tail.push_back(format!("[{stream}] {line}"));
        }

        let status = child.wait().await?;
        let output = tail.into_iter().collect::<Vec<_>>().join("\n");
        let _ = self.recorder.write(
            "terminal.completed",
            json!({
                "toolCallId": invocation.tool_call_id,
                "success": status.success(),
                "exitCode": status.code(),
            }),
        );

        Ok(ToolResult::Text(format!(
            "exit_code={}\n{output}",
            status.code().unwrap_or(-1)
        )))
    }
}

async fn pump_lines(
    stream: &'static str,
    reader: impl tokio::io::AsyncRead + Unpin,
    tx: mpsc::Sender<(&'static str, String)>,
) {
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if tx.send((stream, line)).await.is_err() {
            break;
        }
    }
}

fn host_terminal_tool(recorder: Recorder) -> Tool {
    Tool::new("phase0_terminal")
        .with_description(
            "Run an argv-based process with incremental host-visible stdout and stderr. \
             Prefer this during the GCABB terminal feasibility probe.",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {
                "program": {"type": "string"},
                "args": {"type": "array", "items": {"type": "string"}}
            },
            "required": ["program"],
            "additionalProperties": false
        }))
        .with_handler(Arc::new(HostTerminalTool { recorder }))
}

fn record_rpc<T: Serialize>(
    recorder: &Recorder,
    name: &str,
    started: Instant,
    result: github_copilot_sdk::Result<T>,
) -> Result<()> {
    record_rpc_value(recorder, name, started, result).map(|_| ())
}

fn record_rpc_value<T: Serialize>(
    recorder: &Recorder,
    name: &str,
    started: Instant,
    result: github_copilot_sdk::Result<T>,
) -> Result<Option<T>> {
    match result {
        Ok(value) => {
            recorder.write(
                name,
                json!({"elapsedMs": started.elapsed().as_millis(), "result": &value}),
            )?;
            Ok(Some(value))
        }
        Err(error) => {
            recorder.write(
                &format!("{name}.error"),
                json!({"elapsedMs": started.elapsed().as_millis(), "error": error.to_string()}),
            )?;
            Ok(None)
        }
    }
}

fn record_sdk_event(recorder: &Recorder, event: &github_copilot_sdk::SessionEvent) -> Result<()> {
    let raw = serde_json::to_value(event)?;
    recorder.write("sdk.raw", &raw)?;
    recorder.write("domain.normalized", DomainEvent::from_sdk_event(&raw))
}

async fn exercise_workspace_discovery(
    client: &Client,
    cwd: &Path,
    recorder: &Recorder,
) -> Result<()> {
    let project_paths = Some(vec![cwd.to_string_lossy().into_owned()]);
    let started = Instant::now();
    record_rpc(
        recorder,
        "rpc.agents.discover",
        started,
        client
            .rpc()
            .agents()
            .discover(AgentsDiscoverRequest {
                exclude_host_agents: None,
                project_paths: project_paths.clone(),
            })
            .await,
    )?;
    let started = Instant::now();
    record_rpc(
        recorder,
        "rpc.skills.discover",
        started,
        client
            .rpc()
            .skills()
            .discover(SkillsDiscoverRequest {
                exclude_host_skills: None,
                project_paths: project_paths.clone(),
                skill_directories: None,
            })
            .await,
    )?;
    let started = Instant::now();
    record_rpc(
        recorder,
        "rpc.instructions.discover",
        started,
        client
            .rpc()
            .instructions()
            .discover(InstructionsDiscoverRequest {
                exclude_host_instructions: None,
                project_paths,
            })
            .await,
    )
}
async fn exercise_read_only_rpcs(session: &Session, recorder: &Recorder) -> Result<()> {
    let started = Instant::now();
    record_rpc(
        recorder,
        "rpc.session.model.get_current",
        started,
        session.rpc().model().get_current().await,
    )?;
    let started = Instant::now();
    record_rpc(
        recorder,
        "rpc.session.mode.get",
        started,
        session.rpc().mode().get().await,
    )?;
    let started = Instant::now();
    record_rpc(
        recorder,
        "rpc.session.workspaces.get_workspace",
        started,
        session.rpc().workspaces().get_workspace().await,
    )?;
    let started = Instant::now();
    record_rpc(
        recorder,
        "rpc.session.workspaces.list_files",
        started,
        session.rpc().workspaces().list_files().await,
    )?;
    let started = Instant::now();
    record_rpc(
        recorder,
        "rpc.session.plan.read",
        started,
        session.rpc().plan().read().await,
    )?;
    let started = Instant::now();
    record_rpc(
        recorder,
        "rpc.session.agent.list",
        started,
        session.rpc().agent().list().await,
    )?;
    let started = Instant::now();
    record_rpc(
        recorder,
        "rpc.session.tasks.list",
        started,
        session.rpc().tasks().list().await,
    )
}

/// Records the current queue contents as a compact, order-preserving list so
/// consecutive snapshots in the JSONL output can be diffed by eye.
async fn record_pending_items(session: &Session, recorder: &Recorder, step: &str) -> Result<()> {
    let started = Instant::now();
    let pending = record_rpc_value(
        recorder,
        "rpc.session.queue.pending_items",
        started,
        session.rpc().queue().pending_items().await,
    )?;
    let Some(pending) = pending else {
        return Ok(());
    };
    let order: Vec<Value> = pending
        .items
        .iter()
        .map(|item| {
            json!({
                "id": item.id,
                "kind": item.kind,
                "agentMode": item.agent_mode,
                "displayText": item.display_text,
            })
        })
        .collect();
    recorder.write(
        "queue.order",
        json!({
            "step": step,
            "items": order,
            "steeringMessages": pending.steering_messages,
        }),
    )
}

/// Verifies that an immediate-delivery `session.send` steers a turn that is
/// already running, which is the stable-surface equivalent of
/// `session.queue.sendNow`.
///
/// Sends two real prompts to the model.
async fn exercise_steering(
    session: &Session,
    recorder: &Recorder,
    timeout: Duration,
) -> Result<()> {
    let mut subscription = session.subscribe();
    recorder.write("steering_probe.start", Value::Null)?;

    let long_prompt = "Count slowly from 1 to 40, one number per line, \
                       with a short sentence about each number.";
    let first = session
        .send(long_prompt)
        .await
        .context("failed to send the long-running prompt")?;
    recorder.write("steering_probe.long_send", json!({"messageId": first}))?;

    // Wait until the turn is demonstrably running before interrupting it.
    let deadline = tokio::time::Instant::now() + timeout;
    let mut turn_running = false;
    while let Ok(Ok(event)) = tokio::time::timeout_at(deadline, subscription.recv()).await {
        if matches!(
            event.event_type.as_str(),
            "assistant.message_delta" | "assistant.streaming_delta"
        ) {
            turn_running = true;
            break;
        }
        if event.event_type == "session.idle" {
            break;
        }
    }
    recorder.write(
        "steering_probe.turn_running",
        json!({"running": turn_running}),
    )?;
    if !turn_running {
        recorder.write(
            "steering_probe.aborted",
            json!({"reason": "the first turn never started streaming"}),
        )?;
        return Ok(());
    }

    record_pending_items(session, recorder, "steering-mid-turn").await?;

    let started = Instant::now();
    let steer = session
        .send(
            MessageOptions::from("Stop counting. Reply with exactly: steered".to_owned())
                .with_mode(DeliveryMode::Immediate),
        )
        .await;
    match steer {
        Ok(message_id) => recorder.write(
            "steering_probe.immediate_send",
            json!({
                "elapsedMs": started.elapsed().as_millis(),
                "messageId": message_id,
                "acceptedDuringActiveTurn": true,
            }),
        )?,
        Err(error) => {
            recorder.write(
                "steering_probe.immediate_send.error",
                json!({
                    "elapsedMs": started.elapsed().as_millis(),
                    "error": error.to_string(),
                    "acceptedDuringActiveTurn": false,
                }),
            )?;
            return Ok(());
        }
    }

    record_pending_items(session, recorder, "steering-after-immediate-send").await?;

    let deadline = tokio::time::Instant::now() + timeout;
    let mut event_types = Vec::new();
    let mut saw_steered_reply = false;
    while let Ok(Ok(event)) = tokio::time::timeout_at(deadline, subscription.recv()).await {
        event_types.push(event.event_type.clone());
        if event.event_type == "assistant.message"
            && serde_json::to_value(&event)?
                .to_string()
                .contains("steered")
        {
            saw_steered_reply = true;
        }
        if event.event_type == "session.idle" {
            break;
        }
    }
    recorder.write(
        "steering_probe.complete",
        json!({
            "eventTypes": event_types,
            "sawSteeredReply": saw_steered_reply,
        }),
    )
}

#[async_trait]
impl ElicitationHandler for ProbeCallbacks {
    async fn handle(
        &self,
        session_id: SessionId,
        request_id: RequestId,
        request: ElicitationRequest,
    ) -> ElicitationResult {
        let _ = self.recorder.write(
            "callback.elicitation",
            json!({
                "sessionId": session_id,
                "requestId": request_id,
                "request": request,
                "decision": "cancel"
            }),
        );
        ElicitationResult {
            action: "cancel".to_owned(),
            content: None,
        }
    }
}

#[async_trait]
impl UserInputHandler for ProbeCallbacks {
    async fn handle(
        &self,
        session_id: SessionId,
        question: String,
        choices: Option<Vec<String>>,
        allow_freeform: Option<bool>,
    ) -> Option<UserInputResponse> {
        let _ = self.recorder.write(
            "callback.user_input",
            json!({
                "sessionId": session_id,
                "question": question,
                "choices": choices,
                "allowFreeform": allow_freeform,
                "decision": "unavailable"
            }),
        );
        None
    }
}

#[async_trait]
impl ExitPlanModeHandler for ProbeCallbacks {
    async fn handle(
        &self,
        session_id: SessionId,
        data: github_copilot_sdk::ExitPlanModeData,
    ) -> ExitPlanModeResult {
        let _ = self.recorder.write(
            "callback.exit_plan_mode",
            json!({"sessionId": session_id, "data": data, "decision": "deny"}),
        );
        ExitPlanModeResult {
            approved: false,
            selected_action: None,
            feedback: Some("Phase 0 recorder does not make interactive decisions".to_owned()),
        }
    }
}

#[async_trait]
impl AutoModeSwitchHandler for ProbeCallbacks {
    async fn handle(
        &self,
        session_id: SessionId,
        error_code: Option<String>,
        retry_after_seconds: Option<f64>,
    ) -> AutoModeSwitchResponse {
        let _ = self.recorder.write(
            "callback.auto_mode_switch",
            json!({
                "sessionId": session_id,
                "errorCode": error_code,
                "retryAfterSeconds": retry_after_seconds,
                "decision": "no"
            }),
        );
        AutoModeSwitchResponse::No
    }
}

#[async_trait]
impl SessionHooks for ProbeCallbacks {
    async fn on_hook(&self, event: HookEvent) -> HookOutput {
        let _ = self
            .recorder
            .write("hook", json!({"kind": format!("{event:?}")}));
        HookOutput::None
    }
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();
}

async fn start_client(cwd: &Path, recorder: &Recorder) -> Result<Client> {
    let start = Instant::now();
    let mut client_options = ClientOptions::default();
    client_options.working_directory = cwd.to_owned();
    let client = Client::start(client_options)
        .await
        .context("failed to start the Copilot CLI through the SDK")?;
    let startup_timings = client.startup_timings().map(|timings| {
        json!({
            "programResolveMs": timings.program_resolve_ms,
            "processSpawnMs": timings.process_spawn_ms,
            "portWaitMs": timings.port_wait_ms,
            "transportSetupMs": timings.transport_setup_ms,
            "handshakeMs": timings.handshake_ms,
            "sessionFsMs": timings.session_fs_ms,
            "llmHandlerMs": timings.llm_handler_ms,
            "totalMs": timings.total_ms,
        })
    });
    recorder.write(
        "client.started",
        json!({
            "wallClockMs": start.elapsed().as_millis(),
            "startupTimings": startup_timings,
        }),
    )?;

    let models_started = Instant::now();
    match client.list_models().await {
        Ok(models) => recorder.write(
            "rpc.models.list",
            json!({"elapsedMs": models_started.elapsed().as_millis(), "models": models}),
        )?,
        Err(error) => recorder.write(
            "rpc.models.list.error",
            json!({"elapsedMs": models_started.elapsed().as_millis(), "error": error.to_string()}),
        )?,
    }
    Ok(client)
}

async fn open_session(
    client: &Client,
    args: &Args,
    cwd: &Path,
    recorder: &Recorder,
) -> Result<Session> {
    let callbacks = Arc::new(ProbeCallbacks {
        recorder: recorder.clone(),
    });
    let terminal_tool = host_terminal_tool(recorder.clone());
    let permission_handler: Arc<dyn github_copilot_sdk::handler::PermissionHandler> =
        if args.approve_permissions {
            Arc::new(github_copilot_sdk::handler::ApproveAllHandler)
        } else {
            Arc::new(github_copilot_sdk::handler::DenyAllHandler)
        };

    if let Some(session_id) = args.resume.as_deref() {
        client
            .resume_session(
                ResumeSessionConfig::new(session_id.into())
                    .with_working_directory(cwd)
                    .with_permission_handler(permission_handler)
                    .with_elicitation_handler(callbacks.clone())
                    .with_user_input_handler(callbacks.clone())
                    .with_exit_plan_mode_handler(callbacks.clone())
                    .with_auto_mode_switch_handler(callbacks.clone())
                    .with_hooks(callbacks.clone())
                    .with_tools(vec![terminal_tool]),
            )
            .await
            .context("failed to resume SDK session")
    } else {
        client
            .create_session(
                SessionConfig::default()
                    .with_working_directory(cwd)
                    .with_permission_handler(permission_handler)
                    .with_elicitation_handler(callbacks.clone())
                    .with_user_input_handler(callbacks.clone())
                    .with_exit_plan_mode_handler(callbacks.clone())
                    .with_auto_mode_switch_handler(callbacks.clone())
                    .with_hooks(callbacks)
                    .with_tools(vec![terminal_tool]),
            )
            .await
            .context("failed to create SDK session")
    }
}

async fn run_fleet(
    session: &Session,
    recorder: &Recorder,
    prompt: &str,
    timeout: Duration,
) -> Result<()> {
    let mut subscription = session.subscribe();
    let started = Instant::now();
    record_rpc(
        recorder,
        "rpc.session.fleet.start",
        started,
        session
            .rpc()
            .fleet()
            .start(github_copilot_sdk::rpc::FleetStartRequest {
                prompt: Some(prompt.to_owned()),
            })
            .await,
    )?;
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        tokio::select! {
            event = subscription.recv() => {
                let event = event.context("SDK event subscription closed")?;
                record_sdk_event(recorder, &event)?;
                if event.event_type == "session.idle" || event.event_type == "session.error" {
                    return Ok(());
                }
            }
            () = tokio::time::sleep_until(deadline) => {
                warn!("fleet probe timed out; aborting active session");
                session.abort().await.context("failed to abort fleet probe")?;
                recorder.write("probe.fleet_timeout", json!({"timeoutMs": timeout.as_millis()}))?;
                return Ok(());
            }
        }
    }
}

async fn run_prompt(
    session: &Session,
    recorder: &Recorder,
    prompt: &str,
    abort_after: Option<Duration>,
    timeout: Duration,
) -> Result<()> {
    let mut subscription = session.subscribe();
    let sent_at = Instant::now();
    let mut first_output_recorded = false;
    let message_id = session
        .send(prompt)
        .await
        .context("failed to send prompt")?;
    recorder.write("session.sent", json!({"messageId": message_id}))?;

    let mut abort_at = abort_after.map(|delay| tokio::time::Instant::now() + delay);
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        tokio::select! {
            event = subscription.recv() => {
                let event = event.context("SDK event subscription closed")?;
                record_sdk_event(recorder, &event)?;
                if !first_output_recorded
                    && matches!(
                        event.event_type.as_str(),
                        "assistant.message_start"
                            | "assistant.message_delta"
                            | "assistant.reasoning"
                            | "assistant.reasoning_delta"
                    )
                {
                    first_output_recorded = true;
                    recorder.write(
                        "metric.first_output",
                        json!({
                            "elapsedMs": sent_at.elapsed().as_millis(),
                            "eventType": event.event_type,
                        }),
                    )?;
                }
                if event.event_type == "session.idle" || event.event_type == "session.error" {
                    return Ok(());
                }
            }
            () = async {
                if let Some(abort_at) = abort_at {
                    tokio::time::sleep_until(abort_at).await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {
                abort_at = None;
                info!("aborting session after configured delay");
                session.abort().await.context("failed to abort SDK session")?;
            }
            () = tokio::time::sleep_until(deadline) => {
                warn!("probe timed out; aborting active session");
                session.abort().await.context("failed to abort timed-out SDK session")?;
                recorder.write("probe.timeout", json!({"timeoutMs": timeout.as_millis()}))?;
                return Ok(());
            }
            result = tokio::signal::ctrl_c() => {
                result.context("failed to install Ctrl-C handler")?;
                session.abort().await.context("failed to abort interrupted SDK session")?;
                recorder.write("probe.interrupted", Value::Null)?;
                return Ok(());
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let args = Args::parse();
    let cwd = args
        .cwd
        .canonicalize()
        .with_context(|| format!("working directory does not exist: {}", args.cwd.display()))?;
    let recorder = Recorder::create(&args.output)?;
    recorder.write(
        "probe.start",
        json!({
            "sdkCrateVersion": "1.0.9",
            "sdkProtocolVersion": github_copilot_sdk::SDK_PROTOCOL_VERSION,
            "cwd": cwd,
        }),
    )?;

    let client = start_client(&cwd, &recorder).await?;
    exercise_workspace_discovery(&client, &cwd, &recorder).await?;
    let session = open_session(&client, &args, &cwd, &recorder).await?;
    recorder.write(
        "session.ready",
        json!({"sessionId": session.id(), "capabilities": session.capabilities()}),
    )?;
    let timeout = Duration::from_secs(args.timeout_seconds);
    exercise_read_only_rpcs(&session, &recorder).await?;
    if args.steering_probe {
        exercise_steering(&session, &recorder, timeout).await?;
    }
    recorder.write("sdk.history", session.get_events().await?)?;

    if let Some(prompt) = args.fleet_prompt.as_deref() {
        run_fleet(&session, &recorder, prompt, timeout).await?;
    }
    if let Some(prompt) = args.prompt.as_deref() {
        run_prompt(
            &session,
            &recorder,
            prompt,
            args.abort_after_ms.map(Duration::from_millis),
            timeout,
        )
        .await?;
    }

    recorder.write("session.disconnecting", json!({"sessionId": session.id()}))?;
    session.disconnect().await?;
    client.stop().await?;
    recorder.write("probe.complete", Value::Null)?;
    info!(output = %args.output.display(), "Phase 0 SDK probe complete");
    Ok(())
}
