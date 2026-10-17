#![recursion_limit = "256"]
// SPDX-License-Identifier: Apache-2.0

mod shutdown_signal;

use std::env;
use std::fs;
use std::future::Future;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use winwincode_client_port::private_worker_channel::ProviderExecutionProfile;

use winwincode_codex::{
    ExecutionMode, ObserverMode, ProductionCodexAdapter, ProductionCodexConfig,
    ProductionCodexOptions,
};
use winwincode_domain::{Instant, Sha256Digest, WorkerId, WorkerInstanceId};

use winwincode_execution_port::generated::{
    ModelGatewayRoute, WorkerCapabilityFeature, WorkerCapabilitySet, WorkerCapabilitySetPlatform,
};
use winwincode_worker::managed_session::ManagedSessionConfig;
use winwincode_worker::remote_transport::{RemoteWorkerPort, RemoteWorkerTransportHandle};
use winwincode_worker::validation_artifact::DurableValidationArtifactStore;
use winwincode_worker::workspace_runtime::{JobWorkspaceRuntime, ObservationModelConfiguration};
use winwincode_worker::{WorkerConfig, WorkerLifecycleState, WorkerMain};

const WORKER_TOKIO_STACK_BYTES: usize = 16 * 1024 * 1024;
const WORKER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);
const WORKER_CORE_STOP_TIMEOUT: Duration = Duration::from_secs(5);
const WORKER_TERMINAL_CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);

const USAGE: &str = "usage: winwincode-worker [--check|--managed-session <config-file>|--version]";

fn main() {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("--version") => println!("winwincode-worker {}", env!("CARGO_PKG_VERSION")),
        Some("--check") | None => print_identity(),
        Some("--managed-session") => match args.next() {
            Some(config_path) if args.next().is_none() => {
                run_worker_process(run_managed(&config_path));
            }
            _ => usage_error(),
        },
        Some("--recover-interrupted-calls") => match args.next() {
            Some(config_path) if args.next().is_none() => {
                run_worker_process(recover_interrupted_calls(&config_path));
            }
            _ => usage_error(),
        },
        Some(_) => usage_error(),
    }
}

async fn recover_interrupted_calls(config_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let config = ManagedSessionConfig::read(std::path::Path::new(config_path))?;
    let store = winwincode_provider::ProviderRuntimeStore::open_existing(
        &config.provider_runtime_directory,
    )?;
    store.fail_terminated_worker_calls(&config.worker_instance_id.0)?;
    winwincode_codex::recover_interrupted_tool_calls(
        &config.data_directory.join("codex-runtime/kernel-home"),
    )
    .await?;
    Ok(())
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

struct WorkerBootstrap {
    exit_after_work: bool,
    worker_id: WorkerId,
    worker_instance_id: WorkerInstanceId,
    started_at: Instant,
    data_directory: PathBuf,
    execution_workspace_directory: Option<PathBuf>,
    provider_runtime_directory: PathBuf,
    provider_execution_profile: ProviderExecutionProfile,
    source_directory: PathBuf,
    helper_executable: PathBuf,
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

async fn run_managed(config_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let config = ManagedSessionConfig::read(std::path::Path::new(config_path))?;
    eprintln!(
        "winwincode-worker: managed worker {} instance {}",
        config.worker_id.0, config.worker_instance_id.0
    );
    Box::pin(run_worker(WorkerBootstrap {
        exit_after_work: true,
        worker_id: config.worker_id,
        worker_instance_id: config.worker_instance_id,
        started_at: now_instant()?,
        data_directory: config.data_directory,
        execution_workspace_directory: config.execution_workspace_directory,
        provider_runtime_directory: config.provider_runtime_directory,
        provider_execution_profile: config.provider_execution_profile,
        source_directory: config.source_directory,
        helper_executable: config.helper_executable,
        model_route: config.model_route.unwrap_or_else(default_model_route),
    }))
    .await
}

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

async fn shutdown_worker<Port, Codex>(worker: &mut WorkerMain<Port, Codex>, now: Instant)
where
    Port: winwincode_codex::WorkerExecutionPort,
    Codex: winwincode_codex::CodexCoreAdapter + Send + 'static,
{
    if let Ok(Ok(report)) =
        tokio::time::timeout(WORKER_SHUTDOWN_TIMEOUT, Box::pin(worker.shutdown(now))).await
        && report.codex_failures == 0
    {
        return;
    }
    if !matches!(
        tokio::time::timeout(WORKER_CORE_STOP_TIMEOUT, worker.stop_execution()).await,
        Ok(Ok(()))
    ) {
        eprintln!("winwincode-worker: execution cleanup remains unconfirmed");
    }
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
        provider_runtime_directory,
        provider_execution_profile,
        source_directory,
        helper_executable,
        model_route,
    } = bootstrap;
    #[cfg(feature = "test-support")]
    winwincode_worker::mechanism_timing::configure(data_directory.join("mechanism-timing.jsonl"));
    let capabilities = worker_capabilities()?;
    let execution_mode = configured_execution_mode("WWC_WORKER_EXECUTION_MODE")?;
    let observer_mode = provider_execution_profile.observer_mode;
    let observation_model = configured_observation_model(&provider_execution_profile)?;
    let (port, handle) = RemoteWorkerPort::open(worker_id.clone(), worker_instance_id.clone());
    let execution_source = port.provider_execution_source();
    let extension_source = port.device_extension_source();
    let codex = production_codex(
        &data_directory,
        &provider_runtime_directory,
        &provider_execution_profile,
        extension_source,
        model_route,
        capabilities.clone(),
        execution_mode,
        observer_mode,
        helper_executable,
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
        .with_provider_runtime(&provider_runtime_directory, execution_source)
        .with_observer_mode(observer_mode)
        .with_registration_request_namespace(&worker_instance_id, &started_at)
        .with_model_intake_log(&intake_log);
    if let Some(observation_model) = observation_model {
        worker = worker.with_observation_model(observation_model);
    }

    let registration = tokio::select! {
        result = Box::pin(register_until_active(&mut worker, &handle, &started_at)) => Some(result),
        () = interrupt.wait() => None,
        error = handle.wait_for_terminal_error() => {
            Some(Err(error.into()))
        },
    };
    match registration {
        Some(Ok(())) => {}
        Some(Err(error)) => {
            shutdown_worker(&mut worker, now_instant()?).await;
            return Err(error);
        }
        None => {
            handle.close();
            shutdown_worker(&mut worker, now_instant()?).await;
            return Ok(());
        }
    }

    let _accounting_reconciler = worker
        .execution_port()
        .spawn_accounting_reconciler(provider_runtime_directory);
    let mut drive = tokio::time::interval(Duration::from_millis(25));
    drive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut observed_drive_failures = Vec::new();
    let mut interrupted = false;
    let mut drive_error = None;
    loop {
        tokio::select! {
            () = interrupt.wait() => { interrupted = true; break; },
            _ = handle.wait_for_terminal_error() => { break; },
            _ = drive.tick() => {
                let step = async {
                if worker.transport_failure().is_some() { return Ok(true); }
                #[cfg(feature = "test-support")]
                winwincode_worker::mechanism_timing::record("drive_entry", &serde_json::json!({}));
                if let Some(error) = Box::pin(worker.drive_with_controls(&handle, now_instant)).await? {
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
                let result = tokio::select! {
                    result = interrupt.until_interrupt(step) => result,
                    _ = handle.wait_for_terminal_error() => None,
                };
                let Some(result) = result else { interrupted = handle.terminal_error().is_none(); break; };
                match result {
                    Ok(true) => break,
                    Ok(false) => {},
                    Err(error) => { drive_error = Some(error); break; },
                }
            }
        }
        if handle.terminal_error().is_some()
            || worker.transport_failure().is_some()
            || (worker.required_delivery_failure().is_some() && worker.active_jobs().is_empty())
        {
            break;
        }
    }
    if (interrupted || drive_error.is_some() || worker.transport_failure().is_some())
        && handle.terminal_error().is_none()
    {
        handle.close();
    }
    shutdown_worker(&mut worker, now_instant()?).await;
    if matches!(handle.terminal_error(),
        Some(winwincode_worker::remote_transport::RemoteWorkerPortError::NetworkExhausted(failure))
            if failure.kind == winwincode_network::ErrorKind::AuthorityExpired)
    {
        let _ = tokio::time::timeout(WORKER_TERMINAL_CLEANUP_TIMEOUT, async {
            loop {
                if Box::pin(worker.close_expired_terminal_outbox(now_instant()?)).await == Ok(true)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Ok::<_, Box<dyn std::error::Error>>(())
        })
        .await;
    }
    if interrupted {
        return Ok(());
    }
    if let Some(error) = handle.terminal_error() {
        return Err(error.into());
    }
    if let Some(error) = drive_error {
        return Err(error);
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

#[allow(clippy::too_many_arguments)]
fn production_codex(
    data_directory: &std::path::Path,
    provider_runtime_directory: &std::path::Path,
    profile: &ProviderExecutionProfile,
    extension_source: Arc<dyn winwincode_provider::DeviceExtensionSource>,
    model_route: ModelGatewayRoute,
    capabilities: WorkerCapabilitySet,
    execution_mode: ExecutionMode,
    observer_mode: ObserverMode,
    helper_executable: PathBuf,
) -> Result<ProductionCodexAdapter, Box<dyn std::error::Error>> {
    let provider_id = profile.provider.clone();
    let model_id = profile.model.clone();
    let options = ProductionCodexOptions {
        data_directory: data_directory.join("codex-runtime"),
        helper_executable,
        provider: provider_id,
        model: model_id,
        gateway_route: model_route,
        registered_capabilities: capabilities,
        discovered_capabilities: Vec::new(),
        execution_mode,
        observer_mode,
    };
    let mut config = ProductionCodexConfig::try_new(options)?;
    match env::var("WWC_WORKER_APPROVAL_OWNER").as_deref() {
        Ok("execution_port") => config = config.with_host_action_approvals(),
        Ok("core") | Err(env::VarError::NotPresent) => {}
        _ => return Err("invalid Worker approval owner".into()),
    }
    if let Some(effort) = &profile.reasoning {
        config = config.with_reasoning_effort(effort)?;
    }
    if let Some(settings) = &profile.fusion {
        config = config.with_fusion(settings.clone())?;
    }
    if let Some(provider) = &profile.jev_judge {
        config = config.with_jev_judge(provider.clone())?;
    }
    if let Some(settings) = &profile.jev_context {
        config = config.with_jev_context(settings.clone())?;
    }
    Ok(ProductionCodexAdapter::open(config)?
        .with_provider_runtime(provider_runtime_directory.to_path_buf())?
        .with_device_extension_source(extension_source))
}

#[cfg(test)]
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

fn configured_observation_model(
    profile: &ProviderExecutionProfile,
) -> Result<Option<ObservationModelConfiguration>, Box<dyn std::error::Error>> {
    let required = |value: &Option<String>| {
        value
            .clone()
            .ok_or("Observer model configuration is incomplete")
    };
    match profile.observer_mode {
        ObserverMode::Off => Ok(None),
        ObserverMode::AmbiguousOnly => ObservationModelConfiguration::try_new(
            required(&profile.observer_provider)?,
            required(&profile.observer_model)?,
            ModelGatewayRoute {
                capability: required(&profile.observer_capability)?,
                route: required(&profile.observer_route)?,
            },
        )
        .map(Some)
        .map_err(Into::into),
        ObserverMode::Shadow | ObserverMode::Always => Err(format!(
            "Observer mode {} is not implemented by this Worker release",
            profile.observer_mode.as_config()
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
                provider: Some("local-fixture".into()),
                model: Some("local-fixture".into()),
                gateway_route: default_model_route(),
                registered_capabilities: register.capabilities.clone(),
                discovered_capabilities: Vec::new(),
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
        for (mode, route_expected, error_expected) in [
            (ObserverMode::Off, false, false),
            (ObserverMode::AmbiguousOnly, true, false),
            (ObserverMode::Shadow, false, true),
            (ObserverMode::Always, false, true),
        ] {
            let profile = super::ProviderExecutionProfile {
                observer_mode: mode,
                observer_provider: Some("observer-provider".into()),
                observer_model: Some("observer-model".into()),
                observer_capability: Some("strict-json".into()),
                observer_route: Some("observer-route".into()),
                ..super::ProviderExecutionProfile::default()
            };
            let configured = configured_observation_model(&profile);
            assert_eq!(configured.is_err(), error_expected, "mode={mode:?}");
            assert_eq!(
                configured.as_ref().ok().is_some_and(Option::is_some),
                route_expected,
                "mode={mode:?}"
            );
            if let Ok(Some(configured)) = configured {
                assert_eq!(Some(configured.provider), profile.observer_provider);
                assert_eq!(Some(configured.model), profile.observer_model);
                assert_eq!(
                    Some(configured.route.capability),
                    profile.observer_capability
                );
                assert_eq!(Some(configured.route.route), profile.observer_route);
            }
        }
        assert!(
            configured_observation_model(&super::ProviderExecutionProfile::default())
                .unwrap()
                .is_none()
        );
        assert!(
            configured_observation_model(&super::ProviderExecutionProfile {
                observer_mode: ObserverMode::AmbiguousOnly,
                ..super::ProviderExecutionProfile::default()
            })
            .is_err()
        );
    }
}
