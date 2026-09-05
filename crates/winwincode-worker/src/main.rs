// SPDX-License-Identifier: Apache-2.0

#![recursion_limit = "256"]

//! Standalone Execution Worker process entrypoint.

use std::env;
use std::fs;
use std::future::Future;
use std::path::PathBuf;
use std::time::Duration;

use winwincode_codex::{
    HelperReleaseManifest, ProductionCodexAdapter, ProductionCodexConfig, ProductionCodexOptions,
};
use winwincode_domain::{Instant, Sha256Digest, WorkerId, WorkerInstanceId};
use winwincode_execution_port::action_enforcement::ActionEnforcementSigningKey;
use winwincode_execution_port::action_gateway::ExecutionEnvelopeToken;
use winwincode_execution_port::generated::{
    ModelGatewayRoute, WorkerCapabilityFeature, WorkerCapabilitySet, WorkerCapabilitySetPlatform,
};
use winwincode_worker::managed_session::{ManagedSessionConfig, WorkerSessionCredential};
use winwincode_worker::remote_transport::{RemoteWorkerPort, RemoteWorkerTransportHandle};
use winwincode_worker::workspace_runtime::JobWorkspaceRuntime;
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
    worker_id: WorkerId,
    worker_instance_id: WorkerInstanceId,
    started_at: Instant,
    data_directory: PathBuf,
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
        worker_id: WorkerId(required("WWC_WORKER_ID")?),
        worker_instance_id: WorkerInstanceId(required("WWC_WORKER_INSTANCE_ID")?),
        started_at: env::var("WWC_WORKER_STARTED_AT").map_or(now_instant()?, Instant),
        data_directory: PathBuf::from(required("WWC_WORKER_DATA_DIRECTORY")?),
        source_directory: PathBuf::from(required("WWC_WORKER_SOURCE_ROOT")?),
        server_origin: required("WWC_WORKER_SERVER_ORIGIN")?,
        credential_path: PathBuf::from(required("WWC_WORKER_CREDENTIAL_FILE")?),
        model_route: default_model_route(),
    };
    run_worker(bootstrap).await
}

/// Managed entry (plan §14.4): identity and locality come only from the
/// local mode-0600 config file written by the Device Client. The execution
/// loop below is the exact `--remote` machinery over the same
/// `serverOrigin` `ExecutionPort` exchange; nothing is read from or decided
/// by the Server beyond that existing protocol.
async fn run_managed(config_path: &str) -> Result<(), Box<dyn std::error::Error>> {
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
    run_worker(WorkerBootstrap {
        worker_id: config.worker_id.clone(),
        worker_instance_id: config.worker_instance_id.clone(),
        started_at: now_instant()?,
        data_directory: config.data_directory.clone(),
        source_directory: config.source_directory.clone(),
        server_origin: config.server_origin.clone(),
        credential_path: credential.path().to_path_buf(),
        model_route: config
            .model_route
            .clone()
            .unwrap_or_else(default_model_route),
    })
    .await
}

/// The shared execution main loop. Both entries run the same registration,
/// control drain, heartbeat, and Codex drive cycles over
/// [`RemoteWorkerPort::open`]; only the [`WorkerBootstrap`] source differs.
async fn run_worker(bootstrap: WorkerBootstrap) -> Result<(), Box<dyn std::error::Error>> {
    let WorkerBootstrap {
        worker_id,
        worker_instance_id,
        started_at,
        data_directory,
        source_directory,
        server_origin,
        credential_path,
        model_route,
    } = bootstrap;
    let capabilities = worker_capabilities()?;
    let (port, handle) = RemoteWorkerPort::open(
        &server_origin,
        &fs::read(required("WWC_WORKER_TLS_ROOT_DER_FILE")?)?,
        credential_path,
        worker_id.clone(),
        worker_instance_id.clone(),
        Duration::from_secs(15),
    )?;
    let codex = production_codex(&data_directory, model_route, capabilities.clone())?;
    let workspaces =
        JobWorkspaceRuntime::open(data_directory.join("worker-workspaces"), &source_directory)?;
    let config = WorkerConfig {
        worker_id,
        worker_instance_id: worker_instance_id.clone(),
        started_at: started_at.clone(),
        capabilities,
    };
    let mut worker = WorkerMain::new(config, port, codex, workspaces)
        .with_registration_request_namespace(&worker_instance_id, &started_at);

    while worker.lifecycle() == WorkerLifecycleState::Booting
        || worker.lifecycle() == WorkerLifecycleState::Registering
    {
        if worker.start(started_at.clone()).await.is_ok() {
            Box::pin(drain_controls(&mut worker, &handle)).await?;
        }
        if worker.lifecycle() != WorkerLifecycleState::Active {
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    let mut heartbeat = tokio::time::interval(Duration::from_secs(1));
    let mut drive = tokio::time::interval(Duration::from_millis(25));
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = heartbeat.tick() => {
                let now = now_instant()?;
                if worker.heartbeat(now.clone()).await.is_ok() {
                    Box::pin(drain_controls(&mut worker, &handle)).await?;
                }
            }
            _ = drive.tick() => {
                let now = now_instant()?;
                Box::pin(drain_controls(&mut worker, &handle)).await?;
                let _ = Box::pin(worker.poll_codex(now)).await;
            }
        }
    }
    let _ = Box::pin(worker.shutdown(now_instant()?)).await;
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
    while let Some((delivery_id, message)) = handle.next_control()? {
        match worker.accept_control(&message, now_instant()?).await {
            Ok(()) => handle.confirm(delivery_id)?,
            Err(error) => {
                if env::var_os("WWC_DEBUG_REMOTE_WORKER").is_some() {
                    let kind = serde_json::to_value(&message)
                        .ok()
                        .and_then(|value| {
                            value
                                .get("kind")
                                .and_then(serde_json::Value::as_str)
                                .map(str::to_owned)
                        })
                        .unwrap_or_else(|| "unknown".to_owned());
                    eprintln!(
                        "remote Worker control rejected: kind={kind}; category={:?}",
                        error.code
                    );
                }
                handle.retry(&delivery_id)?;
                return Err(Box::new(error));
            }
        }
    }
    Ok(())
}

fn production_codex(
    data_directory: &std::path::Path,
    model_route: ModelGatewayRoute,
    capabilities: WorkerCapabilitySet,
) -> Result<ProductionCodexAdapter, Box<dyn std::error::Error>> {
    let helper_release_manifest = HelperReleaseManifest::from_file(&PathBuf::from(required(
        "WWC_WORKER_HELPER_RELEASE_MANIFEST",
    )?))?;
    let options = ProductionCodexOptions {
        data_directory: data_directory.join("codex-runtime"),
        helper_executable: PathBuf::from(required("WWC_WORKER_HELPER_EXECUTABLE")?),
        helper_release_manifest,
        provider: required("WWC_WORKER_MODEL_PROVIDER_ID")?,
        model: required("WWC_WORKER_MODEL_ID")?,
        gateway_route: model_route,
        registered_capabilities: capabilities,
        discovered_capabilities: Vec::new(),
        action_signing_key: ActionEnforcementSigningKey::from_bytes(parse_hex_key(&required(
            "WWC_WORKER_ACTION_SIGNING_KEY_HEX",
        )?)?)?,
        execution_envelope: ExecutionEnvelopeToken {
            version: 1,
            digest: Sha256Digest(required("WWC_WORKER_EXECUTION_ENVELOPE_DIGEST")?),
        },
    };
    Ok(ProductionCodexAdapter::open(
        ProductionCodexConfig::try_new(options)?,
    )?)
}

/// Canonical model route used when no managed config override exists —
/// identical to the previous hardcoded `--remote` value.
fn default_model_route() -> ModelGatewayRoute {
    ModelGatewayRoute {
        capability: "reasoning".to_owned(),
        route: "embedded-canonical-remote".to_owned(),
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
