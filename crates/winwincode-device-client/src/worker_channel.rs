// SPDX-License-Identifier: Apache-2.0

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Instant;

use winwincode_client_port::private_worker_channel::{
    MAX_PRIVATE_LINE_BYTES, MAX_PRIVATE_PENDING, PrivateWorkerRequest, PrivateWorkerRequestBody,
    PrivateWorkerResponse, PrivateWorkerResponseBody, ProviderExecutionProfile,
};
use winwincode_client_port::worker_relay::{DeviceWorkerRelayRequest, WorkerRelayRequest};
use winwincode_execution_port::runtime_trace_outbox::ObserverMode;
use winwincode_provider::DeviceProviderError;

use crate::daemon::ExchangeTransport;
use crate::provider_broker::ProviderBroker;

pub(crate) fn provider_execution_profile(
    _directory: &Path,
) -> Result<ProviderExecutionProfile, DeviceProviderError> {
    let read = |key| match std::env::var(key) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(_) => Err(DeviceProviderError::StateInvalid),
    };
    let mut profile = ProviderExecutionProfile {
        provider: read("WWC_WORKER_MODEL_PROVIDER_ID")?,
        model: read("WWC_WORKER_MODEL_ID")?,
        reasoning: read("WWC_WORKER_MODEL_REASONING_EFFORT")?,
        fusion: read("WWC_WORKER_FUSION")?
            .map(|value| serde_json::from_str(&value))
            .transpose()?,
        jev_context: read("WWC_WORKER_JEV_CONTEXT")?
            .map(|value| serde_json::from_str(&value))
            .transpose()?,
        jev_judge: read("WWC_WORKER_JEV_JUDGE")?,
        sealed_benchmark_tools: false,
        observer_mode: ObserverMode::default(),
        observer_provider: None,
        observer_model: None,
        observer_capability: None,
        observer_route: None,
    };
    profile.observer_mode = ObserverMode::from_config(
        read("WWC_WORKER_OBSERVER_MODE")?
            .as_deref()
            .filter(|value| !value.is_empty())
            .unwrap_or("off"),
    )
    .ok_or(DeviceProviderError::StateInvalid)?;
    match profile.observer_mode {
        ObserverMode::Off => {}
        ObserverMode::AmbiguousOnly => {
            let required = |key| -> Result<String, DeviceProviderError> {
                let value = read(key)?.ok_or(DeviceProviderError::StateInvalid)?;
                if value.is_empty() || value.len() > 200 || value.chars().any(char::is_control) {
                    return Err(DeviceProviderError::StateInvalid);
                }
                Ok(value)
            };
            profile.observer_provider = Some(required("WWC_WORKER_OBSERVER_MODEL_PROVIDER_ID")?);
            profile.observer_model = Some(required("WWC_WORKER_OBSERVER_MODEL_ID")?);
            profile.observer_capability = Some(required("WWC_WORKER_OBSERVER_MODEL_CAPABILITY")?);
            profile.observer_route = Some(required("WWC_WORKER_OBSERVER_MODEL_ROUTE")?);
        }
        ObserverMode::Shadow | ObserverMode::Always => {
            return Err(DeviceProviderError::StateInvalid);
        }
    }
    if let Some(value) = read("WWC_BENCHMARK_SEALED_TOOLS")? {
        if value != "1" {
            return Err(DeviceProviderError::StateInvalid);
        }
        profile.sealed_benchmark_tools = true;
    }
    if let Some(fusion) = &profile.fusion {
        winwincode_execution_port::agent_config::validate_fusion_settings(fusion)
            .map_err(|_| DeviceProviderError::StateInvalid)?;
    }
    if let Some(context) = &profile.jev_context {
        winwincode_execution_port::jev_decision::validate_policy(&context.policy)
            .map_err(|_| DeviceProviderError::StateInvalid)?;
    }
    Ok(profile)
}

struct ControlRequest {
    id: u64,
    request: WorkerRelayRequest,
}

fn respond<W: Write>(writer: &Mutex<W>, id: u64, response: PrivateWorkerResponseBody) -> bool {
    let Ok(mut writer) = writer.lock() else {
        return false;
    };
    let encoded = serde_json::to_writer(
        &mut *writer,
        &PrivateWorkerResponse {
            request_id: id,
            response,
        },
    );
    encoded.is_ok() && writer.write_all(b"\n").is_ok() && writer.flush().is_ok()
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn serve<R: Read, W: Write + Send + 'static>(
    output: R,
    input: W,
    endpoint: String,
    transport: Option<Arc<dyn ExchangeTransport>>,
    node: String,
    worker: String,
    instance: String,
    broker: &Arc<ProviderBroker>,
) {
    let writer = Arc::new(Mutex::new(input));
    let (sender, receiver) = std::sync::mpsc::sync_channel::<ControlRequest>(MAX_PRIVATE_PENDING);
    let control_writer = writer.clone();
    let control_broker = broker.clone();
    std::thread::spawn(move || {
        for request in receiver {
            let payload = DeviceWorkerRelayRequest {
                client_node_id: node.clone(),
                worker_id: worker.clone(),
                worker_instance_id: instance.clone(),
                request: request.request,
            };
            let started = Instant::now();
            let response =
                crate::supervisor::relay_worker_request(&endpoint, transport.as_deref(), &payload);
            control_broker.observe(&payload.request, &response, started);
            if !respond(
                &control_writer,
                request.id,
                PrivateWorkerResponseBody::Relay { response },
            ) {
                control_broker.close();
                break;
            }
        }
    });
    let local_pending = Arc::new(AtomicUsize::new(0));
    let mut reader = BufReader::new(output);
    loop {
        let mut line = Vec::new();
        let read = Read::take(&mut reader, (MAX_PRIVATE_LINE_BYTES + 1) as u64)
            .read_until(b'\n', &mut line);
        if !matches!(read, Ok(count) if count > 0 && count <= MAX_PRIVATE_LINE_BYTES) {
            line.fill(0);
            break;
        }
        let decoded = serde_json::from_slice::<PrivateWorkerRequest>(&line);
        line.fill(0);
        let Ok(envelope) = decoded else {
            break;
        };
        let (id, request) = (envelope.request_id, envelope.request);
        if let PrivateWorkerRequestBody::Relay { request } = request {
            if sender.send(ControlRequest { id, request }).is_err() {
                break;
            }
            continue;
        }
        if local_pending.fetch_add(1, Ordering::AcqRel) >= MAX_PRIVATE_PENDING {
            local_pending.fetch_sub(1, Ordering::AcqRel);
            if !respond(
                &writer,
                id,
                crate::provider_broker::local_response(Err(
                    DeviceProviderError::StorageUnavailable,
                )),
            ) {
                break;
            }
            continue;
        }
        let broker = broker.clone();
        let writer = writer.clone();
        let pending = local_pending.clone();
        std::thread::spawn(move || {
            let response = match request {
                PrivateWorkerRequestBody::Provider { operation, body } => {
                    broker.provider(operation, &body)
                }
                PrivateWorkerRequestBody::RefreshExtensions { body } => broker.extensions(&body),
                PrivateWorkerRequestBody::Relay { .. } => unreachable!(),
            };
            let _ = respond(&writer, id, response);
            pending.fetch_sub(1, Ordering::AcqRel);
        });
    }
    broker.close();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use winwincode_client_port::private_worker_channel::ProviderOperation;

    #[cfg(unix)]
    #[test]
    fn pipe_ids_are_correlation_and_need_not_arrive_in_order() {
        let (mut worker, device) = std::os::unix::net::UnixStream::pair().unwrap();
        worker
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let broker = Arc::new(ProviderBroker::new(
            Path::new("").to_path_buf(),
            Path::new("").to_path_buf(),
            "worker".into(),
            "instance".into(),
            None,
            None,
            ProviderExecutionProfile::default(),
            Arc::new(|| true),
        ));
        let server = std::thread::spawn(move || {
            serve(
                device.try_clone().unwrap(),
                device,
                String::new(),
                None,
                String::new(),
                "worker".into(),
                "instance".into(),
                &broker,
            );
        });
        for request_id in [9, 3] {
            serde_json::to_writer(
                &mut worker,
                &PrivateWorkerRequest {
                    request_id,
                    request: PrivateWorkerRequestBody::Provider {
                        operation: ProviderOperation::ResolveConnection,
                        body: "invalid request text".into(),
                    },
                },
            )
            .unwrap();
            worker.write_all(b"\n").unwrap();
        }
        worker.flush().unwrap();
        let mut reader = BufReader::new(worker.try_clone().unwrap());
        let mut ids = Vec::new();
        for _ in 0..2 {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert!(!line.contains("invalid request text"));
            let reply: PrivateWorkerResponse = serde_json::from_str(&line).unwrap();
            ids.push(reply.request_id);
            assert!(matches!(
                reply.response,
                PrivateWorkerResponseBody::Local { error: Some(_), .. }
            ));
        }
        ids.sort_unstable();
        assert_eq!(ids, [3, 9]);
        worker.shutdown(std::net::Shutdown::Both).unwrap();
        server.join().unwrap();
    }
}
