// SPDX-License-Identifier: Apache-2.0

#![recursion_limit = "256"]

//! Standalone Execution Worker process entrypoint.

mod shutdown_signal;

use std::env;
use std::fs;
use std::future::Future;
use std::io::Write as _;
use std::path::PathBuf;
use std::time::Duration;

use winwincode_codex::{
    ExecutionMode, HelperReleaseManifest, ObserverMode, ProductionCodexAdapter,
    ProductionCodexConfig, ProductionCodexOptions,
};
use winwincode_domain::{Instant, Sha256Digest, WorkerId, WorkerInstanceId};
use winwincode_execution_port::action_enforcement::ActionEnforcementSigningKey;
use winwincode_execution_port::action_gateway::ExecutionEnvelopeToken;
use winwincode_execution_port::generated::{
    ModelGatewayRoute, WorkerCapabilityFeature, WorkerCapabilitySet, WorkerCapabilitySetPlatform,
};
use winwincode_worker::managed_session::{ManagedSessionConfig, WorkerSessionCredential};
use winwincode_worker::remote_transport::{RemoteWorkerPort, RemoteWorkerTransportHandle};
use winwincode_worker::validation_artifact::DurableValidationArtifactStore;
use winwincode_worker::workspace_runtime::{JobWorkspaceRuntime, ObservationModelConfiguration};
use winwincode_worker::{WorkerConfig, WorkerLifecycleState, WorkerMain};

const WORKER_TOKIO_STACK_BYTES: usize = 16 * 1024 * 1024;

const USAGE: &str =
    "usage: winwincode-worker [--check|--remote|--managed-session <config-file>|--version]";

fn main() {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("--version") => println!("winwincode-worker {}", env!("CARGO_PKG_VERSION")),
        Some("--check") | None => print_identity(),
        Some("--remote") if args.next().is_none() => {
            run_worker_process(run_remote());
        }
        Some("--managed-session") => match args.next() {
            Some(config_path) if args.next().is_none() => {
                run_worker_process(run_managed(&config_path));
            }
            _ => usage_error(),
        },
        Some(_) => usage_error(),
    }
}

fn usage_error() -> ! {
    eprintln!("{USAGE}");
    std::process::exit(2);
}

fn print_identity() {
    let identity = winwincode_worker::binary_identity();
    if let Ok(json) = serde_json::to_string(&identity) {
        println!("{json}");
    } else {
        eprintln!("Worker identity serialization failed");
        std::process::exit(1);
    }
}

/// Identity and locality inputs the two entries differ on. Everything the
/// execution loop needs beyond this (TLS trust root, helper release,
/// provider model selection, action signing, execution envelope) stays on
/// the existing process environment for both entries.
struct WorkerBootstrap {
    exit_after_work: bool,
    worker_id: WorkerId,
    worker_instance_id: WorkerInstanceId,
    started_at: Instant,
    data_directory: PathBuf,
    execution_workspace_directory: Option<PathBuf>,
    provider_directory: PathBuf,
    source_directory: PathBuf,
    server_origin: String,
    credential_path: PathBuf,
    model_route: ModelGatewayRoute,
}

fn run_worker_process(future: impl Future<Output = Result<(), Box<dyn std::error::Error>>>) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("winwincode-worker")
        .thread_stack_size(WORKER_TOKIO_STACK_BYTES)
        .build()
        .unwrap_or_else(|error| panic!("failed to create Worker runtime: {error}"));
    if let Err(error) = runtime.block_on(future) {
        eprintln!("winwincode-worker: {error}");
        std::process::exit(1);
    }
}

async fn run_remote() -> Result<(), Box<dyn std::error::Error>> {
    let bootstrap = WorkerBootstrap {
        exit_after_work: false,
        worker_id: WorkerId(required("WWC_WORKER_ID")?),
        worker_instance_id: WorkerInstanceId(required("WWC_WORKER_INSTANCE_ID")?),
        started_at: env::var("WWC_WORKER_STARTED_AT").map_or(now_instant()?, Instant),
        data_directory: PathBuf::from(required("WWC_WORKER_DATA_DIRECTORY")?),
        execution_workspace_directory: None,
        provider_directory: PathBuf::from(required("WWC_DEVICE_PROVIDER_DIRECTORY")?),
        source_directory: PathBuf::from(required("WWC_WORKER_SOURCE_ROOT")?),
        server_origin: required("WWC_WORKER_SERVER_ORIGIN")?,
        credential_path: PathBuf::from(required("WWC_WORKER_CREDENTIAL_FILE")?),
        model_route: default_model_route(),
    };
    Box::pin(run_worker(bootstrap)).await
}

/// Managed entry (plan §14.4): identity and locality come only from the
/// local mode-0600 config file written by the Device Client. The execution
/// loop below is the exact `--remote` machinery over the same
/// `serverOrigin` `ExecutionPort` exchange; nothing is read from or decided
/// by the Server beyond that existing protocol.
fn await_managed_start_gate(
    mut input: impl std::io::Read,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut release = [0u8; 1];
    input.read_exact(&mut release)?;
    if release != [1] {
        return Err("managed start gate rejected".into());
    }
    Ok(())
}

async fn run_managed(config_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    if env::var("WWC_MANAGED_START_GATE").as_deref() == Ok("1") {
        await_managed_start_gate(std::io::stdin())?;
    }
    let config = ManagedSessionConfig::read(std::path::Path::new(config_path))?;
    let credential = WorkerSessionCredential::load(&config.worker_credential_path)?;
    eprintln!(
        "winwincode-worker: managed session {} for worker {} instance {} under lease {} with fencing token {} on {} (client {} instance {}); worker session credential digest {}",
        config.worker_session_id.0,
        config.worker_id.0,
        config.worker_instance_id.0,
        config.occupancy_lease_id.0,
        config.occupancy_fencing_token,
        config.repository_binding_id.0,
        config.client_node_id.0,
        config.client_instance_id.0,
        credential.digest().0,
    );
    Box::pin(run_worker(WorkerBootstrap {
        exit_after_work: true,
        worker_id: config.worker_id.clone(),
        worker_instance_id: config.worker_instance_id.clone(),
        started_at: now_instant()?,
        data_directory: config.data_directory.clone(),
        execution_workspace_directory: config.execution_workspace_directory.clone(),
        provider_directory: config.provider_directory.clone(),
        source_directory: config.source_directory.clone(),
        server_origin: config.server_origin.clone(),
        credential_path: credential.path().to_path_buf(),
        model_route: config
            .model_route
            .clone()
            .unwrap_or_else(default_model_route),
    }))
    .await
}

/// The shared execution main loop. Both entries run the same registration,
/// control drain, heartbeat, and Codex drive cycles over
/// [`RemoteWorkerPort::open`]; only the [`WorkerBootstrap`] source differs.
/// Retries Worker registration until the lifecycle leaves the boot/register
/// window. A silent retry loop would look like a hung process in production
/// logs, so each failure prints its secret-safe category only.
async fn register_until_active<Port, Codex>(
    worker: &mut WorkerMain<Port, Codex>,
    handle: &RemoteWorkerTransportHandle,
    started_at: &Instant,
) -> Result<(), Box<dyn std::error::Error>>
where
    Port: winwincode_codex::WorkerExecutionPort,
    Codex: winwincode_codex::CodexCoreAdapter + Send + 'static,
{
    while worker.lifecycle() == WorkerLifecycleState::Booting
        || worker.lifecycle() == WorkerLifecycleState::Registering
    {
        if let Err(error) = Box::pin(worker.start(started_at.clone())).await {
            eprintln!(
                "winwincode-worker: registration/start retry category={:?} reason={} lifecycle={:?}",
                error.code,
                error.reason,
                worker.lifecycle()
            );
        }
        if let Some(error) = worker
            .transport_failure()
            .or_else(|| worker.required_delivery_failure())
        {
            return Err(error.into());
        }
        Box::pin(drain_controls(worker, handle)).await?;
        if let Some(error) = handle.terminal_error() {
            return Err(error.into());
        }
        if worker.lifecycle() != WorkerLifecycleState::Active {
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
    Ok(())
}

#[allow(
    clippy::too_many_lines,
    reason = "the composition owns registration, control delivery, and bounded shutdown"
)]
async fn run_worker(bootstrap: WorkerBootstrap) -> Result<(), Box<dyn std::error::Error>> {
    let mut interrupt = shutdown_signal::WorkerInterrupt::new()?;
    let WorkerBootstrap {
        exit_after_work,
        worker_id,
        worker_instance_id,
        started_at,
        data_directory,
        execution_workspace_directory,
        provider_directory,
        source_directory,
        server_origin,
        credential_path,
        model_route,
    } = bootstrap;
    #[cfg(feature = "test-support")]
    winwincode_worker::mechanism_timing::configure(data_directory.join("mechanism-timing.jsonl"));
    let capabilities = worker_capabilities()?;
    let execution_mode = configured_execution_mode("WWC_WORKER_EXECUTION_MODE")?;
    let observer_mode = configured_observer_mode("WWC_WORKER_OBSERVER_MODE")?;
    let observation_model = configured_observation_model(observer_mode, required)?;
    let (port, handle) = RemoteWorkerPort::open(
        &server_origin,
        &fs::read(required("WWC_WORKER_TLS_ROOT_DER_FILE")?)?,
        credential_path,
        worker_id.clone(),
        worker_instance_id.clone(),
        Duration::from_secs(15),
    )?;
    let _accounting_reconciler = port.spawn_accounting_reconciler(
        provider_directory.clone(),
        ActionEnforcementSigningKey::from_bytes(parse_hex_key(&required(
            "WWC_WORKER_ACTION_SIGNING_KEY_HEX",
        )?)?)?,
    );
    let codex = production_codex(
        &data_directory,
        &provider_directory,
        model_route,
        capabilities.clone(),
        execution_mode,
        observer_mode,
    )?;
    let validation_artifacts =
        DurableValidationArtifactStore::open(data_directory.join("worker-validation-artifacts"))?;
    let workspaces = match execution_workspace_directory {
        Some(root) => JobWorkspaceRuntime::open_partitioned(root, &source_directory)?,
        None => {
            JobWorkspaceRuntime::open(data_directory.join("worker-workspaces"), &source_directory)?
        }
    }
    .with_validation_artifact_port(validation_artifacts);
    let config = WorkerConfig {
        worker_id,
        worker_instance_id: worker_instance_id.clone(),
        started_at: started_at.clone(),
        capabilities,
    };
    let intake_log = data_directory
        .join("codex-runtime")
        .join("model-intake.log");
    let mut worker = WorkerMain::new(config, port, codex, workspaces)
        .with_device_providers(&provider_directory)?
        .with_observer_mode(observer_mode)
        .with_registration_request_namespace(&worker_instance_id, &started_at)
        .with_model_intake_log(&intake_log);
    if let Some(observation_model) = observation_model {
        worker = worker.with_observation_model(observation_model);
    }

    let registration = tokio::select! {
        result = Box::pin(register_until_active(&mut worker, &handle, &started_at)) => Some(result),
        () = interrupt.wait() => None,
    };
    match registration {
        Some(Ok(())) => {}
        Some(Err(error)) => {
            let _ = Box::pin(worker.shutdown(now_instant()?)).await;
            return Err(error);
        }
        None => {
            let _ = Box::pin(worker.shutdown(now_instant()?)).await;
            return Ok(());
        }
    }

    let heartbeat_interval_ms = worker
        .heartbeat_interval_ms()
        .ok_or("registered Worker heartbeat interval is unavailable")?;
    let mut heartbeat = tokio::time::interval(Duration::from_millis(heartbeat_interval_ms));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut drive = tokio::time::interval(Duration::from_millis(25));
    let mut observed_drive_failures = Vec::new();
    loop {
        tokio::select! {
            () = interrupt.wait() => break,
            _ = heartbeat.tick() => {
                let step = async {
                    Box::pin(drain_controls(&mut worker, &handle)).await?;
                    #[cfg(feature = "test-support")]
                    winwincode_worker::mechanism_timing::record("heartbeat_entry", &serde_json::json!({}));
                    let _ = Box::pin(worker.heartbeat(now_instant()?)).await;
                    #[cfg(feature = "test-support")]
                    winwincode_worker::mechanism_timing::record("heartbeat_exit", &serde_json::json!({}));
                    // A refused upstream frame still carries valid controls.
                    Box::pin(drain_controls(&mut worker, &handle)).await?;
                    Ok::<_, Box<dyn std::error::Error>>(())
                };
                let Some(result) = interrupt.until_interrupt(step).await else { break; };
                result?;
            }
            _ = drive.tick() => {
                let step = async {
                if worker.transport_failure().is_some() { return Ok(true); }
                // The shared driver retries one bounded outbox batch and polls
                // Core even when that batch is backpressured. A separate
                // pre-flush must not prevent cancellation facts from draining.
                #[cfg(feature = "test-support")]
                winwincode_worker::mechanism_timing::record("drive_entry", &serde_json::json!({}));
                if let Some(error) = Box::pin(worker.drive_with_controls(&handle, now_instant)).await? {
                    // Retain each finite category once. A trace mismatch also
                    // carries only numeric cursors and identity-match booleans;
                    // never persist raw frames, identities or Provider content.
                    if !observed_drive_failures.contains(&error.code) {
                        observed_drive_failures.push(error.code);
                        if let Ok(mut log) = fs::OpenOptions::new().create(true).append(true).open(&intake_log) {
                            let diagnostic = if error.code == winwincode_worker::WorkerErrorCode::RuntimeTraceMismatch {
                                error.reason.as_str()
                            } else { "" };
                            let _ = writeln!(log, "component=worker stage=drive code={:?} {diagnostic}", error.code);
                        }
                    }
                    if env::var_os("WWC_WORKER_POLL_DEBUG").is_some() {
                        eprintln!("winwincode-worker: Core drive retry failed: {:?}", error.code);
                    }
                }
                #[cfg(feature = "test-support")]
                winwincode_worker::mechanism_timing::record("drive_exit", &serde_json::json!({}));
                Ok::<_, Box<dyn std::error::Error>>(exit_after_work && worker.work_drained())
                };
                let Some(result) = interrupt.until_interrupt(step).await else { break; };
                if result? { break; }
            }
        }
        if handle.terminal_error().is_some()
            || worker.transport_failure().is_some()
            || (worker.required_delivery_failure().is_some() && worker.active_jobs().is_empty())
        {
            break;
        }
    }
    let _ = Box::pin(worker.shutdown(now_instant()?)).await;
    if let Some(error) = handle.terminal_error() {
        return Err(error.into());
    }
    if let Some(error) = worker
        .transport_failure()
        .or_else(|| worker.required_delivery_failure())
    {
        return Err(error.into());
    }
    Ok(())
}

async fn drain_controls<Port, Codex>(
    worker: &mut WorkerMain<Port, Codex>,
    handle: &RemoteWorkerTransportHandle,
) -> Result<(), Box<dyn std::error::Error>>
where
    Port: winwincode_codex::WorkerExecutionPort,
    Codex: winwincode_codex::CodexCoreAdapter + Send + 'static,
{
    worker.drain_controls(handle, now_instant).await
}

fn production_codex(
    data_directory: &std::path::Path,
    provider_directory: &std::path::Path,
    model_route: ModelGatewayRoute,
    capabilities: WorkerCapabilitySet,
    execution_mode: ExecutionMode,
    observer_mode: ObserverMode,
) -> Result<ProductionCodexAdapter, Box<dyn std::error::Error>> {
    let helper_release_manifest = HelperReleaseManifest::from_file(&PathBuf::from(required(
        "WWC_WORKER_HELPER_RELEASE_MANIFEST",
    )?))?;
    let store = winwincode_provider::DeviceProviderStore::open(provider_directory)?;
    let snapshot = store.snapshot("local")?;
    let extensions = store.restore_extensions(&data_directory.join("codex-runtime/kernel-home"))?;
    let mut discovered_capabilities = Vec::new();
    for server in extensions {
        for operation in winwincode_execution_port::mcp_resource::McpResourceOperation::ALL {
            discovered_capabilities.push(
                winwincode_execution_port::capability_adapter::CapabilityDescriptor::mcp_resource(
                    &server.server,
                    operation,
                    server.digest.trim_start_matches("sha256:"),
                    winwincode_execution_port::capability_adapter::CapabilityHealth::Healthy,
                    winwincode_execution_port::capability_adapter::CapabilityOrigin::CodexCoreMcp,
                )?,
            );
        }
        for tool in server.tools {
            discovered_capabilities.push(
                winwincode_execution_port::capability_adapter::CapabilityDescriptor::mcp(
                    &server.server,
                    &tool,
                    server.digest.trim_start_matches("sha256:"),
                    winwincode_execution_port::capability_adapter::CapabilityHealth::Healthy,
                    winwincode_execution_port::capability_adapter::CapabilityOrigin::CodexCoreMcp,
                )?,
            );
        }
    }
    let default_provider = snapshot
        .providers
        .iter()
        .find(|provider| provider.config.enabled && provider.credential_configured)
        .ok_or("configure a Provider on this Device before starting a Worker")?;
    let provider_id = env::var("WWC_WORKER_MODEL_PROVIDER_ID")
        .unwrap_or_else(|_| default_provider.config.provider_id.clone());
    let model_id = env::var("WWC_WORKER_MODEL_ID")
        .unwrap_or_else(|_| default_provider.config.model_ids[0].clone());
    let options = ProductionCodexOptions {
        data_directory: data_directory.join("codex-runtime"),
        helper_executable: PathBuf::from(required("WWC_WORKER_HELPER_EXECUTABLE")?),
        helper_release_manifest,
        provider: provider_id,
        model: model_id,
        gateway_route: model_route,
        registered_capabilities: capabilities,
        discovered_capabilities,
        action_signing_key: ActionEnforcementSigningKey::from_bytes(parse_hex_key(&required(
            "WWC_WORKER_ACTION_SIGNING_KEY_HEX",
        )?)?)?,
        execution_envelope: ExecutionEnvelopeToken {
            version: 1,
            digest: Sha256Digest(required("WWC_WORKER_EXECUTION_ENVELOPE_DIGEST")?),
        },
        execution_mode,
        observer_mode,
    };
    let mut config = ProductionCodexConfig::try_new(options)?;
    match env::var("WWC_WORKER_APPROVAL_OWNER").as_deref() {
        Ok("execution_port") => config = config.with_host_action_approvals(),
        Ok("core") | Err(env::VarError::NotPresent) => {}
        _ => return Err("invalid Worker approval owner".into()),
    }
    if let Some(value) = env::var_os("WWC_BENCHMARK_SEALED_TOOLS") {
        if value != "1" {
            return Err("invalid benchmark sealed tools".into());
        }
        config = config.with_sealed_benchmark_tools();
    }
    if let Some(effort) = env::var_os("WWC_WORKER_MODEL_REASONING_EFFORT") {
        config =
            config.with_reasoning_effort(effort.to_str().ok_or("invalid reasoning effort")?)?;
    }
    if let Some(settings) = configured_fusion(env::var_os("WWC_WORKER_FUSION").as_deref())? {
        config = config.with_fusion(settings)?;
    }
    let judge = env::var_os("WWC_WORKER_JEV_JUDGE");
    let context = env::var_os("WWC_WORKER_JEV_CONTEXT");
    let file = env::var_os("WWC_DEVICE_JEV_SETTINGS_FILE");
    if let Some(provider) = configured_jev_judge(&store, judge.as_deref(), file.as_deref())? {
        config = config.with_jev_judge(provider)?;
    }
    if let Some(settings) = configured_jev_context(
        &store,
        context.as_deref(),
        if judge.is_some() && context.is_none() {
            None
        } else {
            file.as_deref()
        },
    )? {
        config = config.with_jev_context(settings)?;
    }
    Ok(ProductionCodexAdapter::open(config)?
        .with_device_extensions(provider_directory.to_path_buf())?)
}

fn configured_fusion(
    profile: Option<&std::ffi::OsStr>,
) -> Result<
    Option<winwincode_execution_port::agent_config::AgentFusionSettings>,
    Box<dyn std::error::Error>,
> {
    let Some(profile) = profile else {
        return Ok(None);
    };
    let settings = serde_json::from_str(profile.to_str().ok_or("invalid Fusion profile")?)
        .map_err(|_| "invalid Fusion profile")?;
    winwincode_execution_port::agent_config::validate_fusion_settings(&settings)
        .map_err(|_| "invalid Fusion members")?;
    Ok(Some(settings))
}

fn configured_jev_judge(
    store: &winwincode_provider::DeviceProviderStore,
    profile: Option<&std::ffi::OsStr>,
    settings_file: Option<&std::ffi::OsStr>,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let Some(profile) = profile else {
        return Ok(None);
    };
    let provider = profile.to_str().ok_or("invalid JEV Judge provider")?;
    if provider.is_empty()
        || provider.len() > 200
        || !provider
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:".contains(&byte))
    {
        return Err("invalid JEV Judge provider".into());
    }
    if let Some(path) = settings_file {
        store.import_jev_settings(std::path::Path::new(path), provider)?;
    }
    store.resolve_jev_settings(provider)?;
    Ok(Some(provider.to_owned()))
}

fn configured_jev_context(
    store: &winwincode_provider::DeviceProviderStore,
    profile: Option<&std::ffi::OsStr>,
    settings_file: Option<&std::ffi::OsStr>,
) -> Result<
    Option<winwincode_execution_port::agent_config::AgentJevContextSettings>,
    Box<dyn std::error::Error>,
> {
    let Some(profile) = profile else {
        if settings_file.is_some() {
            return Err("JEV settings file requires WWC_WORKER_JEV_CONTEXT".into());
        }
        return Ok(None);
    };
    let settings: winwincode_execution_port::agent_config::AgentJevContextSettings =
        serde_json::from_str(profile.to_str().ok_or("invalid JEV context profile")?)
            .map_err(|_| "invalid JEV context profile")?;
    winwincode_execution_port::jev_decision::validate_policy(&settings.policy)
        .map_err(|_| "invalid JEV context policy")?;
    if let Some(path) = settings_file {
        store.import_jev_settings(std::path::Path::new(path), &settings.provider)?;
    }
    store.resolve_jev_settings(&settings.provider)?;
    Ok(Some(settings))
}

/// Canonical model route used when no managed config override exists —
/// identical to the previous hardcoded `--remote` value.
fn default_model_route() -> ModelGatewayRoute {
    ModelGatewayRoute {
        capability: "reasoning".to_owned(),
        route: "embedded-canonical-remote".to_owned(),
    }
}

fn configured_execution_mode(name: &str) -> Result<ExecutionMode, Box<dyn std::error::Error>> {
    let value = optional_configuration(name, "react")?;
    let mode = ExecutionMode::from_config(&value)
        .ok_or_else(|| format!("{name} contains an unsupported execution mode"))?;
    released_worker_execution_mode_required(mode)?;
    Ok(mode)
}

fn released_worker_execution_mode_required(mode: ExecutionMode) -> Result<(), &'static str> {
    match mode {
        ExecutionMode::React
        | ExecutionMode::DelegatedPatchShadow
        | ExecutionMode::DelegatedPatch => Ok(()),
        ExecutionMode::DebugProbe => {
            Err("WWC_WORKER_EXECUTION_MODE selects DebugProbe before runtime routing is available")
        }
    }
}

fn configured_observer_mode(name: &str) -> Result<ObserverMode, Box<dyn std::error::Error>> {
    let value = optional_configuration(name, "off")?;
    ObserverMode::from_config(&value)
        .ok_or_else(|| format!("{name} contains an unsupported observer mode").into())
}

fn configured_observation_model<Read>(
    observer_mode: ObserverMode,
    mut read: Read,
) -> Result<Option<ObservationModelConfiguration>, Box<dyn std::error::Error>>
where
    Read: FnMut(&str) -> Result<String, Box<dyn std::error::Error>>,
{
    match observer_mode {
        ObserverMode::Off => Ok(None),
        ObserverMode::AmbiguousOnly => ObservationModelConfiguration::try_new(
            read("WWC_WORKER_OBSERVER_MODEL_PROVIDER_ID")?,
            read("WWC_WORKER_OBSERVER_MODEL_ID")?,
            ModelGatewayRoute {
                capability: read("WWC_WORKER_OBSERVER_MODEL_CAPABILITY")?,
                route: read("WWC_WORKER_OBSERVER_MODEL_ROUTE")?,
            },
        )
        .map(Some)
        .map_err(Into::into),
        ObserverMode::Shadow | ObserverMode::Always => Err(format!(
            "Observer mode {} is not implemented by this Worker release",
            observer_mode.as_config()
        )
        .into()),
    }
}

fn optional_configuration(name: &str, default: &str) -> Result<String, Box<dyn std::error::Error>> {
    match env::var(name) {
        Ok(value) if !value.is_empty() => Ok(value),
        Ok(_) | Err(env::VarError::NotPresent) => Ok(default.to_owned()),
        Err(error) => Err(error.into()),
    }
}

fn worker_capabilities() -> Result<WorkerCapabilitySet, Box<dyn std::error::Error>> {
    let platform = match (env::consts::ARCH, env::consts::OS) {
        ("aarch64", "macos") => WorkerCapabilitySetPlatform::Aarch64AppleDarwin,
        ("x86_64", "macos") => WorkerCapabilitySetPlatform::X8664AppleDarwin,
        ("aarch64", "linux") => WorkerCapabilitySetPlatform::Aarch64UnknownLinuxGnu,
        ("x86_64", "linux") => WorkerCapabilitySetPlatform::X8664UnknownLinuxGnu,
        _ => return Err("unsupported Worker platform".into()),
    };
    Ok(WorkerCapabilitySet {
        capability_digest: Sha256Digest(format!("sha256:{}", "0".repeat(64))),
        features: vec![
            WorkerCapabilityFeature::ArtifactStream,
            WorkerCapabilityFeature::Approval,
            WorkerCapabilityFeature::Git,
            WorkerCapabilityFeature::InteractiveInput,
            WorkerCapabilityFeature::Mcp,
            WorkerCapabilityFeature::ModelProxy,
            WorkerCapabilityFeature::Sandbox,
            WorkerCapabilityFeature::Shell,
        ],
        max_concurrent_jobs: 1,
        platform,
    })
}

fn required(name: &str) -> Result<String, Box<dyn std::error::Error>> {
    env::var(name).map_err(|_| format!("required environment variable {name} is missing").into())
}

fn parse_hex_key(value: &str) -> Result<[u8; 32], Box<dyn std::error::Error>> {
    if value.len() != 64 {
        return Err("Worker action signing key must contain 32 bytes".into());
    }
    let mut result = [0_u8; 32];
    for (index, slot) in result.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
            .map_err(|_| "Worker action signing key is not hexadecimal")?;
    }
    Ok(result)
}

fn now_instant() -> Result<Instant, Box<dyn std::error::Error>> {
    let duration = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?;
    let seconds = i64::try_from(duration.as_secs())?;
    let days = seconds.div_euclid(86_400);
    let second_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = second_of_day / 3_600;
    let minute = second_of_day % 3_600 / 60;
    let second = second_of_day % 60;
    Ok(Instant(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{:03}Z",
        duration.subsec_millis()
    )))
}

fn civil_from_days(days_since_unix_epoch: i64) -> (i64, i64, i64) {
    let days = days_since_unix_epoch + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::{
        ExecutionMode, ObserverMode, configured_observation_model,
        released_worker_execution_mode_required,
    };

    #[cfg(feature = "test-support")]
    #[tokio::test]
    #[allow(
        clippy::too_many_lines,
        reason = "registration integration uses the released helper and persistent adapter for both transport failure classes"
    )]
    async fn registration_returns_permanent_worker_failure_and_retries_only_transient_transport() {
        use super::*;
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        use winwincode_codex::{ExecutionPortFailureKind, WorkerExecutionPort};
        use winwincode_execution_port::generated::{ExecutionPortMessage, WorkerRegisterMessage};
        struct FailingPort {
            permanent: bool,
            calls: Arc<AtomicUsize>,
        }
        impl WorkerExecutionPort for FailingPort {
            type Error = std::io::Error;
            async fn send(&mut self, _: ExecutionPortMessage) -> Result<(), Self::Error> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Err(std::io::Error::from(if self.permanent {
                    std::io::ErrorKind::InvalidInput
                } else {
                    std::io::ErrorKind::ConnectionRefused
                }))
            }
            fn failure_kind(error: &Self::Error) -> ExecutionPortFailureKind {
                if error.kind() == std::io::ErrorKind::InvalidInput {
                    ExecutionPortFailureKind::MessageRejected
                } else {
                    ExecutionPortFailureKind::Unavailable
                }
            }
        }
        for permanent in [true, false] {
            let root = tempfile::tempdir().unwrap();
            let executable = env::current_exe().unwrap();
            let helper = executable
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("winwincode-kernel-helper");
            let fixture: serde_json::Value = serde_json::from_str(include_str!(
                "../../../tests/fixtures/contracts/execution-port.valid.json"
            ))
            .unwrap();
            let register: WorkerRegisterMessage = serde_json::from_value(
                fixture["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|message| message["kind"] == "worker.register")
                    .unwrap()
                    .clone(),
            )
            .unwrap();
            let config = ProductionCodexConfig::try_new(ProductionCodexOptions {
                data_directory: root.path().join("codex"),
                helper_executable: helper.clone(),
                helper_release_manifest: HelperReleaseManifest::from_test_helper(&helper).unwrap(),
                provider: "local-fixture".into(),
                model: "local-fixture".into(),
                gateway_route: default_model_route(),
                registered_capabilities: register.capabilities.clone(),
                discovered_capabilities: Vec::new(),
                action_signing_key: ActionEnforcementSigningKey::from_bytes([31; 32]).unwrap(),
                execution_envelope: ExecutionEnvelopeToken {
                    version: 1,
                    digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
                },
                execution_mode: ExecutionMode::React,
                observer_mode: ObserverMode::Off,
            })
            .unwrap();
            let adapter = ProductionCodexAdapter::open(config).unwrap();
            let calls = Arc::new(AtomicUsize::new(0));
            fs::create_dir_all(root.path().join("sources")).unwrap();
            let runtime = JobWorkspaceRuntime::open(
                root.path().join("workspaces"),
                root.path().join("sources"),
            )
            .unwrap();
            let mut worker = WorkerMain::new(
                WorkerConfig {
                    worker_id: register.worker_id,
                    worker_instance_id: register.worker_instance_id,
                    started_at: register.started_at,
                    capabilities: register.capabilities,
                },
                FailingPort {
                    permanent,
                    calls: Arc::clone(&calls),
                },
                adapter,
                runtime,
            );
            let handle = RemoteWorkerTransportHandle::empty_for_test();
            let result = tokio::time::timeout(
                Duration::from_millis(800),
                Box::pin(register_until_active(
                    &mut worker,
                    &handle,
                    &register.sent_at,
                )),
            )
            .await;
            if permanent {
                let error = result
                    .expect("permanent refusal must exit the registration loop")
                    .unwrap_err();
                assert_eq!(
                    error
                        .downcast_ref::<winwincode_worker::WorkerError>()
                        .unwrap()
                        .code,
                    winwincode_worker::WorkerErrorCode::ExecutionMessageRejected
                );
                assert_eq!(calls.load(Ordering::SeqCst), 1);
                assert!(!worker.has_pending_durable_evidence());
            } else {
                assert!(result.is_err(), "a transient disconnect remains retryable");
                assert!(calls.load(Ordering::SeqCst) >= 2);
                assert!(worker.required_delivery_failure().is_none());
            }
            assert_eq!(worker.lifecycle(), WorkerLifecycleState::Registering);
        }
    }

    #[test]
    fn managed_start_gate_requires_the_registered_parent_release() {
        assert!(super::await_managed_start_gate(std::io::Cursor::new(Vec::<u8>::new())).is_err());
        assert!(super::await_managed_start_gate(std::io::Cursor::new(vec![0])).is_err());
        assert!(super::await_managed_start_gate(std::io::Cursor::new(vec![1])).is_ok());
    }

    #[test]
    fn fusion_startup_validates_exact_members_without_echoing_input() {
        use std::ffi::OsStr;
        assert!(super::configured_fusion(None).unwrap().is_none());
        let mut profile = serde_json::json!({"members":[
            {"id":"glm","provider":"zhipu-glm","model":"glm-5.3-flash","reasoning":"max"},
            {"id":"mimo","provider":"xiaomi-mimo","model":"mimo-v2.6-pro","reasoning":"max"},
            {"id":"ds","provider":"deepseek","model":"deepseek-flash","reasoning":"max"},
            {"id":"qwen","provider":"qwen","model":"qwen3.8-flash","reasoning":"max"}
        ]});
        let parse = |value: &serde_json::Value| {
            super::configured_fusion(Some(OsStr::new(&value.to_string())))
        };
        let settings = parse(&profile).unwrap().unwrap();
        assert_eq!(serde_json::to_value(settings).unwrap(), profile);
        profile["members"][1] = profile["members"][0].clone();
        assert!(parse(&profile).is_err());
        for input in ["", "private-key", "{\"apiKey\":\"private-key\"}"] {
            let error = super::configured_fusion(Some(OsStr::new(input))).unwrap_err();
            assert!(!error.to_string().contains("private-key"));
        }
    }

    #[test]
    fn jev_startup_requires_private_matching_settings() {
        use std::ffi::OsStr;
        use std::os::unix::fs::{PermissionsExt, symlink};
        use winwincode_provider::DeviceProviderStore;

        let directory = tempfile::tempdir().unwrap();
        let store = DeviceProviderStore::open(&directory.path().join("device")).unwrap();
        let path = directory.path().join("jev.toml");
        let text = "providerId = 'device-jev'\nendpoint = 'https://nli.invalid/score'\napiKey = 'test-private-key'\ntimeoutMs = 1000\nretries = 0\n";
        std::fs::write(&path, text).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let profile = serde_json::json!({
            "provider": "device-jev",
            "policy": {
                "version": "jev-policy.v1", "minimumConfidence": 0.55,
                "pinThreshold": 0.9, "keepThreshold": 0.75, "compactThreshold": 0.75,
                "dropThreshold": 0.9, "taskMemoryThreshold": 0.4,
                "projectMemoryThreshold": 0.6, "longTermMemoryThreshold": 0.8,
            }
        });
        let serialized = profile.to_string();
        let profile_arg = Some(OsStr::new(&serialized));
        let configure = |profile, file| super::configured_jev_context(&store, profile, file);
        assert!(configure(None, None).unwrap().is_none());
        assert!(configure(None, Some(path.as_os_str())).is_err());
        assert!(configure(profile_arg, None).is_err());
        assert!(configure(Some(OsStr::new("")), Some(path.as_os_str())).is_err());
        let configured = configure(profile_arg, Some(path.as_os_str()))
            .unwrap()
            .unwrap();
        assert_eq!(configured.provider, "device-jev");
        assert_eq!(
            super::configured_jev_judge(&store, Some(OsStr::new("device-jev")), None).unwrap(),
            Some("device-jev".into())
        );
        assert!(
            super::configured_jev_judge(&store, Some(OsStr::new("other-provider")), None).is_err()
        );
        assert!(
            super::configured_jev_judge(&store, Some(OsStr::new("device-jev\n")), None).is_err()
        );
        assert!(
            super::configured_jev_judge(&store, None, None)
                .unwrap()
                .is_none()
        );
        assert_eq!(configure(profile_arg, None).unwrap(), Some(configured));
        let mut invalid = profile.clone();
        invalid["apiKey"] = "do-not-print-this".into();
        let invalid_secret = invalid.to_string();
        let error = configure(Some(OsStr::new(&invalid_secret)), None).unwrap_err();
        assert!(!error.to_string().contains("do-not-print-this"));
        invalid = profile;
        invalid["policy"]["dropThreshold"] = 2.0.into();
        let invalid_policy = invalid.to_string();
        assert!(configure(Some(OsStr::new(&invalid_policy)), None).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(configure(profile_arg, Some(path.as_os_str())).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = directory.path().join("link.toml");
        symlink(&path, &link).unwrap();
        assert!(configure(profile_arg, Some(link.as_os_str())).is_err());
        for bad in [
            text.replace("device-jev", "other-provider"),
            "broken secret".into(),
            "x".repeat(65_537),
        ] {
            std::fs::write(&path, bad).unwrap();
            assert!(configure(profile_arg, Some(path.as_os_str())).is_err());
            assert_eq!(
                store
                    .resolve_jev_settings("device-jev")
                    .unwrap()
                    .api_key
                    .as_deref(),
                Some("test-private-key")
            );
        }
        drop(store);
        let reopened = DeviceProviderStore::open(&directory.path().join("device")).unwrap();
        assert!(
            super::configured_jev_context(&reopened, profile_arg, None)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn execution_modes_fail_closed_until_debug_probe_routing_exists() {
        for (mode, expected) in [
            (ExecutionMode::React, Ok(())),
            (ExecutionMode::DelegatedPatchShadow, Ok(())),
            (ExecutionMode::DelegatedPatch, Ok(())),
            (ExecutionMode::DebugProbe, Err(())),
        ] {
            assert_eq!(
                released_worker_execution_mode_required(mode).map_err(|_| ()),
                expected,
                "mode={mode:?}"
            );
        }
    }

    #[test]
    fn observer_modes_have_one_closed_release_configuration() {
        for (mode, expected_reads, route_expected, error_expected) in [
            (ObserverMode::Off, 0, false, false),
            (ObserverMode::AmbiguousOnly, 4, true, false),
            (ObserverMode::Shadow, 0, false, true),
            (ObserverMode::Always, 0, false, true),
        ] {
            let mut reads = Vec::new();
            let configured = configured_observation_model(mode, |name| {
                reads.push(name.to_owned());
                Ok(match name {
                    "WWC_WORKER_OBSERVER_MODEL_PROVIDER_ID" => "observer-provider",
                    "WWC_WORKER_OBSERVER_MODEL_ID" => "observer-model",
                    "WWC_WORKER_OBSERVER_MODEL_CAPABILITY" => "strict-json",
                    "WWC_WORKER_OBSERVER_MODEL_ROUTE" => "observer-route",
                    _ => unreachable!("unexpected Observer environment name"),
                }
                .to_owned())
            });
            assert_eq!(configured.is_err(), error_expected, "mode={mode:?}");
            assert_eq!(
                configured.as_ref().ok().is_some_and(Option::is_some),
                route_expected,
                "mode={mode:?}"
            );
            assert_eq!(reads.len(), expected_reads, "mode={mode:?}");
        }
    }
}
