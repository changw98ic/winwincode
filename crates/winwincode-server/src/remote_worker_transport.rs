// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};
use winwincode_control_plane::{
    ProductStateStorage, RemoteWorkerPoolAdapter, RemoteWorkerPoolError, RemoteWorkerPoolErrorKind,
    RemoteWorkerPrincipal,
};
use winwincode_domain::{ExecutionMessageId, Instant, RepositoryScope, WorkerId, WorkerInstanceId};
use winwincode_execution_port::generated::ExecutionPortMessage;
use winwincode_execution_port::transport::{
    EndpointSide, ExecutionPortCore, FrameDirection, FrameError, RemoteExchangeDelivery,
    RemoteExchangeRequest, RemoteExchangeResponse, RemoteTransportAdapter, TypedFrame,
    execution_message_id,
};
use winwincode_storage::{
    ExecutionLeaseRecord, ExecutionRegistry, SqliteStorage, WorkerPoolId, WorkerRegistryScope,
};

use crate::{RepositoryRuntimeScheduler, RuntimeControlOutbound, RuntimeSupervisorError};

const MAX_PENDING_REMOTE_DELIVERIES: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RemoteWorkerTransportError {
    message: &'static str,
    status: u16,
}

impl RemoteWorkerTransportError {
    #[must_use]
    pub const fn unavailable() -> Self {
        Self::new("remote Worker transport is unavailable")
    }

    const fn new(message: &'static str) -> Self {
        Self {
            message,
            status: 503,
        }
    }
    const fn invalid_frame() -> Self {
        Self {
            message: "remote Worker frame is invalid",
            status: 400,
        }
    }
    #[allow(
        clippy::needless_pass_by_value,
        reason = "owned error callback for Result::map_err"
    )]
    fn frame_error(error: FrameError) -> Self {
        if matches!(error, FrameError::TooLarge) {
            Self {
                message: "remote Worker message exceeds wire budget",
                status: 413,
            }
        } else {
            Self::invalid_frame()
        }
    }
    const fn invalid_route() -> Self {
        Self {
            message: "Worker route does not match its Device session",
            status: 400,
        }
    }
    fn from_pool(error: RemoteWorkerPoolError) -> Self {
        match error.kind() {
            RemoteWorkerPoolErrorKind::InvalidConnection
            | RemoteWorkerPoolErrorKind::UnsupportedMessage => Self {
                message: "remote Worker frame rejected",
                status: 400,
            },
            RemoteWorkerPoolErrorKind::Registry => {
                Self::new("remote Worker authority is unavailable")
            }
        }
    }
    #[must_use]
    pub const fn status_code(&self) -> u16 {
        self.status
    }
}

impl fmt::Display for RemoteWorkerTransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for RemoteWorkerTransportError {}

pub trait RemoteWorkerExchangePort: Send + Sync {
    fn recover_accepted_outcomes(&self) -> Result<usize, RemoteWorkerTransportError>;
    fn accounting(
        &self,
        _client_node_id: &str,
        _worker_id: &str,
        _worker_instance_id: &str,
        _body: Option<&[u8]>,
        _offset: u64,
        _now: Instant,
    ) -> Result<Vec<u8>, RemoteWorkerTransportError> {
        Err(RemoteWorkerTransportError::unavailable())
    }
    fn exchange(
        &self,
        client_node_id: &str,
        worker_id: &str,
        worker_instance_id: &str,
        request_body: &[u8],
        now: Instant,
    ) -> Result<Vec<u8>, RemoteWorkerTransportError>;
}

type RemoteDelivery = (WorkerId, WorkerInstanceId, ExecutionMessageId, Vec<u8>);

#[derive(Default)]
struct RemoteDeliveryQueue {
    pending: Mutex<Vec<RemoteDelivery>>,
    confirmed: Mutex<HashMap<(WorkerId, WorkerInstanceId), ConfirmedControls>>,
}

#[derive(Default)]
struct ConfirmedControls {
    ids: std::collections::HashSet<ExecutionMessageId>,
    order: std::collections::VecDeque<ExecutionMessageId>,
}

const MAX_CONFIRMED_CONTROLS: usize = 4_096;

impl ConfirmedControls {
    fn insert(&mut self, id: ExecutionMessageId) {
        if self.ids.insert(id.clone()) {
            self.order.push_back(id);
            while self.order.len() > MAX_CONFIRMED_CONTROLS {
                if let Some(old) = self.order.pop_front() {
                    self.ids.remove(&old);
                }
            }
        }
    }
}

fn confirmed_redelivery_frame(frame: &[u8]) -> bool {
    RemoteTransportAdapter::<NoopCore>::decode(frame)
        .is_ok_and(|frame| suppress_confirmed_redelivery(frame.message()))
}

fn suppress_confirmed_redelivery(message: &ExecutionPortMessage) -> bool {
    matches!(message, ExecutionPortMessage::JobCancelMessage(_))
}

impl RemoteDeliveryQueue {
    fn retain_live_leases(
        &self,
        registry: &ExecutionRegistry<'_>,
        now: &Instant,
    ) -> Result<(), RemoteWorkerTransportError> {
        let mut pending = self
            .pending
            .lock()
            .map_err(|_| RemoteWorkerTransportError::unavailable())?;
        let retained = pending
            .iter()
            .map(|(_, _, _, frame)| queued_execution_is_live(frame, registry, now))
            .collect::<Result<Vec<_>, _>>()?;
        let mut retained = retained.into_iter();
        pending.retain(|_| retained.next().unwrap_or(false));
        Ok(())
    }

    fn retain_active_instances(
        &self,
        active: &std::collections::HashSet<(WorkerId, WorkerInstanceId)>,
    ) -> Result<(), RemoteWorkerTransportError> {
        self.pending
            .lock()
            .map_err(|_| RemoteWorkerTransportError::unavailable())?
            .retain(|(worker, instance, _, _)| {
                active.contains(&(worker.clone(), instance.clone()))
            });
        self.confirmed
            .lock()
            .map_err(|_| RemoteWorkerTransportError::unavailable())?
            .retain(|identity, _| active.contains(identity));
        Ok(())
    }
    fn prepare_ingress(
        &self,
        request: &RemoteExchangeRequest,
    ) -> Result<bool, RemoteWorkerTransportError> {
        self.acknowledge(
            request.worker_id(),
            request.worker_instance_id(),
            request.acknowledgements(),
        )?;
        let pending = self.pending.lock().map_err(|_| {
            RemoteWorkerTransportError::new("remote Worker delivery queue is unavailable")
        })?;
        let can_accept = pending
            .iter()
            .filter(|(worker, instance, _, _)| {
                worker == request.worker_id() && instance == request.worker_instance_id()
            })
            .count()
            <= MAX_PENDING_REMOTE_DELIVERIES
                - winwincode_execution_port::transport::MAX_REMOTE_DELIVERIES;
        drop(pending);
        Ok(can_accept)
    }
    fn retire_other_instances(
        &self,
        worker: &WorkerId,
        instance: &WorkerInstanceId,
    ) -> Result<(), RemoteWorkerTransportError> {
        let mut pending = self.pending.lock().map_err(|_| {
            RemoteWorkerTransportError::new("remote Worker delivery queue is unavailable")
        })?;
        pending.retain(|(target, boot, _, _)| target != worker || boot == instance);
        drop(pending);
        self.confirmed
            .lock()
            .map_err(|_| {
                RemoteWorkerTransportError::new("remote Worker delivery queue is unavailable")
            })?
            .retain(|(target, boot), _| target != worker || boot == instance);
        Ok(())
    }
    fn acknowledge(
        &self,
        worker_id: &WorkerId,
        instance_id: &WorkerInstanceId,
        ids: &[ExecutionMessageId],
    ) -> Result<(), RemoteWorkerTransportError> {
        let mut pending = self.pending.lock().map_err(|_| {
            RemoteWorkerTransportError::new("remote Worker delivery queue is unavailable")
        })?;
        let mut confirmed = self.confirmed.lock().map_err(|_| {
            RemoteWorkerTransportError::new("remote Worker delivery queue is unavailable")
        })?;
        for id in ids {
            pending.retain(|(target, instance, pending_id, frame)| {
                let matched = target == worker_id && instance == instance_id && pending_id == id;
                if matched && confirmed_redelivery_frame(frame) {
                    confirmed
                        .entry((worker_id.clone(), instance_id.clone()))
                        .or_default()
                        .insert(id.clone());
                }
                !matched
            });
        }
        Ok(())
    }

    #[cfg(test)]
    fn snapshot(
        &self,
        worker_id: &WorkerId,
        instance_id: &WorkerInstanceId,
    ) -> Result<Vec<RemoteExchangeDelivery>, RemoteWorkerTransportError> {
        self.snapshot_at(
            worker_id,
            instance_id,
            &Instant("2026-10-15T00:00:00.000Z".into()),
        )
    }

    fn snapshot_at(
        &self,
        worker_id: &WorkerId,
        instance_id: &WorkerInstanceId,
        server_time: &Instant,
    ) -> Result<Vec<RemoteExchangeDelivery>, RemoteWorkerTransportError> {
        let pending = self.pending.lock().map_err(|_| {
            RemoteWorkerTransportError::new("remote Worker delivery queue is unavailable")
        })?;
        let mut deliveries = Vec::new();
        let mut bytes =
            RemoteExchangeResponse::with_acceptance_at(Vec::new(), false, server_time.clone())
                .and_then(|response| response.encode())
                .map_err(RemoteWorkerTransportError::frame_error)?
                .len();
        for (_, _, delivery_id, frame) in pending
            .iter()
            .filter(|(target, instance, _, _)| target == worker_id && instance == instance_id)
        {
            let delivery = RemoteExchangeDelivery {
                delivery_id: delivery_id.clone(),
                frame: frame.clone(),
            };
            let size = serde_json::to_vec(&delivery)
                .map_err(|_| RemoteWorkerTransportError::invalid_frame())?
                .len()
                + usize::from(!deliveries.is_empty());
            if deliveries.len() == winwincode_execution_port::transport::MAX_REMOTE_DELIVERIES
                || bytes + size > winwincode_execution_port::transport::MAX_REMOTE_RESPONSE_BYTES
            {
                break;
            }
            bytes += size;
            deliveries.push(delivery);
        }
        Ok(deliveries)
    }

    fn enqueue_for(
        &self,
        worker_id: &WorkerId,
        instance_id: &WorkerInstanceId,
        mut message: ExecutionPortMessage,
    ) -> Result<(), RuntimeSupervisorError> {
        separate_heartbeat_receipt_identity(&mut message)?;
        let delivery_id = execution_message_id(&message).map_err(|_| remote_queue_failure())?;
        if suppress_confirmed_redelivery(&message)
            && self
                .confirmed
                .lock()
                .map_err(|_| remote_queue_failure())?
                .get(&(worker_id.clone(), instance_id.clone()))
                .is_some_and(|confirmed| confirmed.ids.contains(&delivery_id))
        {
            return Ok(());
        }
        let frame = TypedFrame::new(FrameDirection::ControlPlaneToWorker, message)
            .and_then(|frame| RemoteTransportAdapter::<NoopCore>::encode(&frame))
            .map_err(|_| remote_queue_failure())?;
        RemoteExchangeResponse::new(vec![RemoteExchangeDelivery {
            delivery_id: delivery_id.clone(),
            frame: frame.clone(),
        }])
        .map_err(|_| remote_queue_failure())?;
        let mut pending = self.pending.lock().map_err(|_| remote_queue_failure())?;
        if pending.iter().any(|(target, instance, id, _)| {
            target == worker_id && instance == instance_id && id == &delivery_id
        }) {
            return Ok(());
        }
        if pending
            .iter()
            .filter(|(target, instance, _, _)| target == worker_id && instance == instance_id)
            .count()
            >= MAX_PENDING_REMOTE_DELIVERIES
        {
            return Err(remote_queue_failure());
        }
        pending.push((worker_id.clone(), instance_id.clone(), delivery_id, frame));
        Ok(())
    }
}

fn queued_execution_is_live(
    frame: &[u8],
    registry: &ExecutionRegistry<'_>,
    now: &Instant,
) -> Result<bool, RemoteWorkerTransportError> {
    let frame = RemoteTransportAdapter::<NoopCore>::decode(frame)
        .map_err(RemoteWorkerTransportError::frame_error)?;
    let (lease, payload_digest) = match frame.message() {
        ExecutionPortMessage::JobDispatchMessage(message) => {
            (&message.lease, Some(&message.job.payload_digest))
        }
        ExecutionPortMessage::LeaseRenewMessage(message) => (&message.lease, None),
        _ => return Ok(true),
    };
    if lease.expires_at.0 <= now.0 {
        return Ok(false);
    }
    let current = registry
        .load_live_lease(&lease.job_id, now)
        .map_err(|_| RemoteWorkerTransportError::unavailable())?;
    let Some(current) = current else {
        return Ok(false);
    };
    let period = ExecutionLeaseRecord {
        job_id: lease.job_id.clone(),
        lease_id: lease.lease_id.clone(),
        payload_digest: payload_digest.unwrap_or(&current.payload_digest).clone(),
        worker_id: lease.worker_id.clone(),
        worker_instance_id: lease.worker_instance_id.clone(),
        attempt: u64::try_from(lease.attempt)
            .map_err(|_| RemoteWorkerTransportError::invalid_frame())?,
        fencing_token: lease.fencing_token.clone(),
        issued_at: lease.issued_at.clone(),
        expires_at: lease.expires_at.clone(),
    };
    registry
        .load_live_lease_for_period(&period, now)
        .map(|lease| lease.is_some())
        .map_err(|_| RemoteWorkerTransportError::unavailable())
}

fn separate_heartbeat_receipt_identity(
    message: &mut ExecutionPortMessage,
) -> Result<(), RuntimeSupervisorError> {
    use winwincode_execution_port::generated::{
        ExecutionPortErrorCode, WorkerHeartbeatAckMessageStatus,
    };
    let ExecutionPortMessage::WorkerHeartbeatAckMessage(ack) = message else {
        return Ok(());
    };
    let class = match ack.status {
        WorkerHeartbeatAckMessageStatus::Accepted | WorkerHeartbeatAckMessageStatus::Duplicate
            if ack.error.is_none() =>
        {
            "accepted"
        }
        WorkerHeartbeatAckMessageStatus::RejectedWorkerInstance
            if ack.error.as_ref().is_some_and(|error| {
                error.code == ExecutionPortErrorCode::SequenceGap && error.retryable
            }) =>
        {
            "gap"
        }
        _ => return Ok(()),
    };
    let identity = serde_json::to_vec(&(
        "worker-heartbeat-receipt-v1",
        &ack.message_id,
        &ack.worker_id,
        &ack.worker_instance_id,
        &ack.heartbeat_sequence,
        class,
    ))
    .map_err(|_| remote_queue_failure())?;
    ack.message_id = ExecutionMessageId(format!(
        "xmsg_{}",
        crate::runtime::crockford_26(&Sha256::digest(identity))
    ));
    Ok(())
}

struct WorkerDeliveryQueue<'queue> {
    queue: &'queue RemoteDeliveryQueue,
    worker_id: WorkerId,
    instance_id: WorkerInstanceId,
}

impl RuntimeControlOutbound for WorkerDeliveryQueue<'_> {
    fn enqueue_control(&self, message: ExecutionPortMessage) -> Result<(), RuntimeSupervisorError> {
        self.queue
            .enqueue_for(&self.worker_id, &self.instance_id, message)
    }
}

fn remote_queue_failure() -> RuntimeSupervisorError {
    RuntimeSupervisorError::transport_unavailable()
}

struct NoopCore;

impl ExecutionPortCore for NoopCore {
    type Output = ();
    type Error = std::convert::Infallible;

    fn accept(&mut self, _message: &ExecutionPortMessage) -> Result<Self::Output, Self::Error> {
        Ok(())
    }
}

pub struct ProductionRemoteWorkerExchange<Core> {
    exchange_gate: Mutex<()>,
    data_directory: PathBuf,
    repository_scope: RepositoryScope,
    scheduler: Mutex<RepositoryRuntimeScheduler>,
    core: Mutex<Core>,
    queue: RemoteDeliveryQueue,
    clock: Arc<dyn crate::StandaloneApplicationClock>,
}

impl<Core> ProductionRemoteWorkerExchange<Core> {
    fn reclaim_terminated_queues(&self, now: &Instant) -> Result<(), RemoteWorkerTransportError> {
        let active = SqliteStorage::open(&self.data_directory)
            .map_err(|_| RemoteWorkerTransportError::unavailable())?
            .active_remote_worker_instances(&now.0)
            .map_err(|_| RemoteWorkerTransportError::unavailable())?
            .into_iter()
            .map(|(worker, instance)| (WorkerId(worker), WorkerInstanceId(instance)))
            .collect();
        self.queue.retain_active_instances(&active)
    }

    #[must_use]
    pub fn new(
        data_directory: impl Into<PathBuf>,
        repository_scope: RepositoryScope,
        scheduler: RepositoryRuntimeScheduler,
        core: Core,
    ) -> Self {
        Self {
            exchange_gate: Mutex::new(()),
            data_directory: data_directory.into(),
            repository_scope,
            scheduler: Mutex::new(scheduler),
            core: Mutex::new(core),
            queue: RemoteDeliveryQueue::default(),
            clock: Arc::new(crate::SystemStandaloneApplicationClock),
        }
    }

    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn crate::StandaloneApplicationClock>) -> Self {
        self.clock = clock;
        self
    }
}

impl<Core> RemoteWorkerExchangePort for ProductionRemoteWorkerExchange<Core>
where
    Core: ExecutionPortCore<Output = Vec<ExecutionPortMessage>> + Send,
    Core::Error: Send + fmt::Display,
{
    fn recover_accepted_outcomes(&self) -> Result<usize, RemoteWorkerTransportError> {
        let _gate = self.exchange_gate.lock().map_err(|_| {
            RemoteWorkerTransportError::new("remote Worker exchange is unavailable")
        })?;
        let now = self.clock.now_instant();
        self.reclaim_terminated_queues(&now)?;
        self.scheduler
            .lock()
            .map_err(|_| RemoteWorkerTransportError::new("remote Worker scheduler is unavailable"))?
            .recover_accepted_outcomes()
            .map_err(|_| RemoteWorkerTransportError::new("accepted outcome recovery failed"))
    }

    fn accounting(
        &self,
        client_node_id: &str,
        worker_id: &str,
        worker_instance_id: &str,
        body: Option<&[u8]>,
        offset: u64,
        now: Instant,
    ) -> Result<Vec<u8>, RemoteWorkerTransportError> {
        let mut storage = SqliteStorage::open(&self.data_directory)
            .map_err(|_| RemoteWorkerTransportError::new("accounting storage unavailable"))?;
        let owner = storage
            .worker_launch_grant_ledger()
            .map_err(|_| RemoteWorkerTransportError::unavailable())?
            .client_node_for_worker(worker_id, worker_instance_id)
            .map_err(|_| RemoteWorkerTransportError::unavailable())?;
        if owner.as_deref() != Some(client_node_id) {
            return Err(RemoteWorkerTransportError::invalid_route());
        }
        if let Some(body) = body {
            let statement =
                winwincode_execution_port::accounting::AttemptAccountingStatement::decode(body)
                    .map_err(|_| RemoteWorkerTransportError::invalid_frame())?;
            statement
                .validate()
                .map_err(|_| RemoteWorkerTransportError::invalid_frame())?;
            if statement.lease.worker_id.0 != worker_id
                || statement.lease.worker_instance_id.0 != worker_instance_id
            {
                return Err(RemoteWorkerTransportError::invalid_frame());
            }
            if !accounting_lease_matches_device(&mut storage, &statement.lease, client_node_id)? {
                return Err(RemoteWorkerTransportError::invalid_route());
            }
            storage
                .reconcile_provider_attempt(&statement, &now)
                .map_err(|error| {
                    if error.kind() == winwincode_storage::StorageErrorKind::InvalidInput {
                        RemoteWorkerTransportError::invalid_frame()
                    } else {
                        RemoteWorkerTransportError::new("accounting import unavailable")
                    }
                })?;
            Ok(b"{}".to_vec())
        } else {
            let mut leases = storage.pending_accounting_leases(offset).map_err(|_| {
                RemoteWorkerTransportError::new("accounting lease query unavailable")
            })?;
            let next_offset = if leases.len() == 100 {
                Some(
                    offset
                        .checked_add(100)
                        .ok_or_else(RemoteWorkerTransportError::invalid_frame)?,
                )
            } else {
                None
            };
            let mut leases_for_device = Vec::new();
            for lease in leases {
                if lease.worker_id.0 == worker_id
                    && lease.worker_instance_id.0 == worker_instance_id
                    && accounting_lease_matches_device(&mut storage, &lease, client_node_id)?
                {
                    leases_for_device.push(lease);
                }
            }
            leases = leases_for_device;
            serde_json::to_vec(
                &winwincode_execution_port::accounting::PendingAccountingPage {
                    leases,
                    next_offset,
                },
            )
            .map_err(|_| RemoteWorkerTransportError::invalid_frame())
        }
    }

    fn exchange(
        &self,
        client_node_id: &str,
        worker_id: &str,
        worker_instance_id: &str,
        request_body: &[u8],
        now: Instant,
    ) -> Result<Vec<u8>, RemoteWorkerTransportError> {
        let _gate = self.exchange_gate.lock().map_err(|_| {
            RemoteWorkerTransportError::new("remote Worker exchange is unavailable")
        })?;
        self.reclaim_terminated_queues(&now)?;
        let request = RemoteExchangeRequest::decode(request_body)
            .map_err(RemoteWorkerTransportError::frame_error)?;
        if request.worker_id().0 != worker_id
            || request.worker_instance_id().0 != worker_instance_id
        {
            return Err(RemoteWorkerTransportError::invalid_frame());
        }
        let frame = RemoteTransportAdapter::<NoopCore>::decode(request.frame())
            .map_err(RemoteWorkerTransportError::frame_error)?;
        let mut storage = SqliteStorage::open(&self.data_directory).map_err(|_| {
            RemoteWorkerTransportError::new("remote Worker Registry is unavailable")
        })?;
        let (responses, principal, frame_accepted) =
            self.accept_ingress(&mut storage, client_node_id, &request, &frame, &now)?;
        Box::new(storage)
            .close()
            .map_err(|_| RemoteWorkerTransportError::new("remote Worker Registry close failed"))?;

        if responses.iter().any(|message|matches!(message,ExecutionPortMessage::WorkerRegistrationResultMessage(result)
            if result.error.is_none() && matches!(result.status,winwincode_execution_port::generated::WorkerRegistrationResultMessageStatus::Accepted|winwincode_execution_port::generated::WorkerRegistrationResultMessageStatus::Duplicate))) {
            self.queue.retire_other_instances(request.worker_id(),request.worker_instance_id())?;
        }

        self.queue.acknowledge(
            request.worker_id(),
            request.worker_instance_id(),
            request.acknowledgements(),
        )?;
        for response in responses {
            self.queue
                .enqueue_for(request.worker_id(), request.worker_instance_id(), response)
                .map_err(|_| RemoteWorkerTransportError::new("remote Worker queue is full"))?;
        }
        let mut scheduler = self.scheduler.lock().map_err(|_| {
            RemoteWorkerTransportError::new("remote Worker scheduler is unavailable")
        })?;
        scheduler
            .acknowledge_remote(
                &now,
                request.worker_id(),
                request.worker_instance_id(),
                request.acknowledgements(),
            )
            .map_err(|_| RemoteWorkerTransportError::new("remote Worker scheduler failed"))?;
        let drive_result = if frame_accepted {
            let worker_queue = WorkerDeliveryQueue {
                queue: &self.queue,
                worker_id: request.worker_id().clone(),
                instance_id: request.worker_instance_id().clone(),
            };
            scheduler.drive_remote_for(
                &now,
                &worker_queue,
                request.worker_id().clone(),
                request.worker_instance_id().clone(),
                principal.worker_pool_id().clone(),
            )
        } else {
            Ok(())
        };
        if let Err(error) = drive_result
            && std::env::var_os("WWC_DEBUG_RUNTIME").is_some()
        {
            eprintln!("remote Worker scheduler drive after exchange failed: {error}");
        }
        let server_time = self.clock.now_instant();
        let mut storage = SqliteStorage::open(&self.data_directory)
            .map_err(|_| RemoteWorkerTransportError::unavailable())?;
        self.queue.retain_live_leases(
            &storage
                .execution_registry()
                .map_err(|_| RemoteWorkerTransportError::unavailable())?,
            &server_time,
        )?;
        let deliveries = self.queue.snapshot_at(
            request.worker_id(),
            request.worker_instance_id(),
            &server_time,
        )?;
        let response = if request.supports_acceptance_receipt() {
            RemoteExchangeResponse::with_acceptance_at(deliveries, frame_accepted, server_time)
        } else {
            RemoteExchangeResponse::new_at(deliveries, server_time)
        }
        .map_err(RemoteWorkerTransportError::frame_error)?;
        response
            .encode()
            .map_err(|_| RemoteWorkerTransportError::new("remote Worker response is invalid"))
    }
}

fn accounting_lease_matches_device(
    storage: &mut SqliteStorage,
    lease: &winwincode_execution_port::generated::ExecutionLeaseStamp,
    node: &str,
) -> Result<bool, RemoteWorkerTransportError> {
    let owner = storage
        .worker_launch_grant_ledger()
        .map_err(|_| RemoteWorkerTransportError::new("accounting device authority unavailable"))?
        .client_node_for_worker(&lease.worker_id.0, &lease.worker_instance_id.0)
        .map_err(|_| RemoteWorkerTransportError::new("accounting device authority unavailable"))?;
    Ok(owner.as_deref() == Some(node))
}

impl<Core> ProductionRemoteWorkerExchange<Core>
where
    Core: ExecutionPortCore<Output = Vec<ExecutionPortMessage>> + Send,
    Core::Error: Send + fmt::Display,
{
    fn route_principal(
        &self,
        storage: &mut SqliteStorage,
        client_node_id: &str,
        worker_id: &str,
        worker_instance_id: &str,
    ) -> Result<RemoteWorkerPrincipal, RemoteWorkerTransportError> {
        let grant = storage
            .worker_launch_grant_ledger()
            .map_err(|_| RemoteWorkerTransportError::unavailable())?
            .active_grant_for_worker(worker_id, worker_instance_id)
            .map_err(|_| RemoteWorkerTransportError::unavailable())?
            .ok_or_else(RemoteWorkerTransportError::invalid_route)?;
        if grant.client_node_id != client_node_id {
            return Err(RemoteWorkerTransportError::invalid_route());
        }
        let occupancy = winwincode_control_plane::ClientOccupancyService::new(storage)
            .active_lease_for_node(client_node_id)
            .map_err(|_| RemoteWorkerTransportError::unavailable())?
            .ok_or_else(RemoteWorkerTransportError::invalid_route)?;
        let allowed = match occupancy.state {
            winwincode_control_plane::OccupancyLeaseState::Occupied
            | winwincode_control_plane::OccupancyLeaseState::Draining => true,
            winwincode_control_plane::OccupancyLeaseState::RecoveryPending => {
                grant.state == winwincode_storage::WorkerLaunchGrantState::Consumed
            }
            _ => false,
        };
        if !allowed
            || occupancy.occupancy_lease_id != grant.occupancy_lease_id
            || occupancy.fencing_token != grant.occupancy_fencing_token
        {
            return Err(RemoteWorkerTransportError::invalid_route());
        }
        let scope = WorkerRegistryScope::Repository {
            organization_id: self.repository_scope.organization_id.clone(),
            workspace_id: self.repository_scope.workspace_id.clone(),
            project_id: self.repository_scope.project_id.clone(),
            repository_id: self.repository_scope.repository_id.clone(),
        };
        RemoteWorkerPrincipal::new(
            WorkerId(worker_id.to_owned()),
            WorkerInstanceId(worker_instance_id.to_owned()),
            WorkerPoolId(
                if grant.work_run_id.is_some() {
                    winwincode_control_plane::STRONGFLOW_DEVICE_WORKER_POOL_ID
                } else {
                    winwincode_control_plane::QUICK_DEVICE_WORKER_POOL_ID
                }
                .to_owned(),
            ),
            scope,
            client_node_id.to_owned(),
            grant.worker_session_id,
        )
        .map_err(RemoteWorkerTransportError::from_pool)
    }

    #[allow(clippy::too_many_lines)]
    fn accept_ingress(
        &self,
        storage: &mut SqliteStorage,
        client_node_id: &str,
        request: &RemoteExchangeRequest,
        frame: &TypedFrame,
        now: &Instant,
    ) -> Result<(Vec<ExecutionPortMessage>, RemoteWorkerPrincipal, bool), RemoteWorkerTransportError>
    {
        let frame_identity = match frame.message() {
            ExecutionPortMessage::WorkerRegisterMessage(message) => {
                Some((&message.worker_id, &message.worker_instance_id))
            }
            ExecutionPortMessage::WorkerHeartbeatMessage(message) => {
                Some((&message.worker_id, &message.worker_instance_id))
            }
            ExecutionPortMessage::JobDispatchResultMessage(m) => {
                Some((&m.lease.worker_id, &m.lease.worker_instance_id))
            }
            ExecutionPortMessage::SessionBindingMessage(m) => {
                Some((&m.lease.worker_id, &m.lease.worker_instance_id))
            }
            ExecutionPortMessage::RuntimeEventMessage(m) => {
                Some((&m.lease.worker_id, &m.lease.worker_instance_id))
            }
            ExecutionPortMessage::ArtifactOpenMessage(m) => {
                Some((&m.lease.worker_id, &m.lease.worker_instance_id))
            }
            ExecutionPortMessage::ArtifactChunkMessage(m) => {
                Some((&m.lease.worker_id, &m.lease.worker_instance_id))
            }
            ExecutionPortMessage::ModelOpenMessage(m) => {
                Some((&m.lease.worker_id, &m.lease.worker_instance_id))
            }
            ExecutionPortMessage::ModelChunkMessage(m) => {
                Some((&m.lease.worker_id, &m.lease.worker_instance_id))
            }
            ExecutionPortMessage::ModelAckMessage(m) => {
                Some((&m.lease.worker_id, &m.lease.worker_instance_id))
            }
            ExecutionPortMessage::InputRequestMessage(m) => {
                Some((&m.lease.worker_id, &m.lease.worker_instance_id))
            }
            ExecutionPortMessage::ApprovalRequestMessage(m) => {
                Some((&m.lease.worker_id, &m.lease.worker_instance_id))
            }
            ExecutionPortMessage::JobCancelAckMessage(m) => {
                Some((&m.lease.worker_id, &m.lease.worker_instance_id))
            }
            ExecutionPortMessage::JobOutcomeMessage(m) => {
                Some((&m.lease.worker_id, &m.lease.worker_instance_id))
            }
            ExecutionPortMessage::ActionEnforcementRequestMessage(m) => {
                Some((&m.lease.worker_id, &m.lease.worker_instance_id))
            }
            ExecutionPortMessage::SnapshotFreezeRequestMessage(m) => {
                Some((&m.lease.worker_id, &m.lease.worker_instance_id))
            }
            ExecutionPortMessage::SnapshotFreezeReceiptMessage(m) => {
                Some((&m.lease.worker_id, &m.lease.worker_instance_id))
            }
            _ => None,
        };
        if frame_identity.is_some_and(|(worker, instance)| {
            worker != request.worker_id() || instance != request.worker_instance_id()
        }) {
            return Err(RemoteWorkerTransportError::invalid_frame());
        }
        let principal = self.route_principal(
            storage,
            client_node_id,
            &request.worker_id().0,
            &request.worker_instance_id().0,
        )?;
        let mut pool = RemoteWorkerPoolAdapter::new(storage);
        let mut connection = if matches!(
            frame.message(),
            ExecutionPortMessage::WorkerRegisterMessage(_)
        ) {
            pool.connect(principal.clone())
        } else {
            pool.resume(
                principal.clone(),
                request.worker_id(),
                request.worker_instance_id(),
            )
            .map_err(RemoteWorkerTransportError::from_pool)?
        };
        let can_accept = self.queue.prepare_ingress(request)?;
        if !can_accept {
            if request.supports_acceptance_receipt() {
                return Ok((Vec::new(), principal, false));
            }
            return Err(RemoteWorkerTransportError::new(
                "remote Worker response backlog must be acknowledged",
            ));
        }

        let responses = if matches!(
            frame.message(),
            ExecutionPortMessage::WorkerRegisterMessage(_)
        ) {
            Ok(vec![
                pool.accept(&mut connection, frame.message(), now)
                    .map_err(RemoteWorkerTransportError::from_pool)?,
            ])
        } else {
            let (worker_id, instance_id) = match frame.message() {
                ExecutionPortMessage::WorkerHeartbeatMessage(heartbeat) => {
                    (&heartbeat.worker_id, &heartbeat.worker_instance_id)
                }
                _ => (request.worker_id(), request.worker_instance_id()),
            };
            pool.authorize_registered_message(&mut connection, worker_id, instance_id, now)
                .map_err(RemoteWorkerTransportError::from_pool)?;
            let mut core = self.core.lock().map_err(|_| {
                RemoteWorkerTransportError::new("remote Worker ingress is unavailable")
            })?;
            log_dispatch_result(frame.message());
            let encoded = RemoteTransportAdapter::<NoopCore>::encode(frame)
                .map_err(|_| RemoteWorkerTransportError::invalid_frame())?;
            RemoteTransportAdapter::new(&mut *core, EndpointSide::ControlPlane)
                .accept(&encoded)
                .map_err(|error| {
                    log_ingress_rejection(frame, &error);
                    RemoteWorkerTransportError::new("remote Worker ingress rejected a frame")
                })
        }?;
        Ok((responses, principal, true))
    }
}

fn log_dispatch_result(message: &ExecutionPortMessage) {
    if std::env::var_os("WWC_DEBUG_RUNTIME").is_some()
        && let ExecutionPortMessage::JobDispatchResultMessage(result) = message
    {
        eprintln!(
            "remote Worker dispatch result status: {:?}; error: {:?}",
            result.status, result.error
        );
    }
}

fn log_ingress_rejection<Error>(frame: &TypedFrame, error: &Error)
where
    Error: fmt::Display,
{
    if std::env::var_os("WWC_DEBUG_RUNTIME").is_none() {
        return;
    }
    let kind = serde_json::to_value(frame.message())
        .ok()
        .and_then(|value| {
            value
                .get("kind")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "unknown".to_owned());
    eprintln!("remote Worker ingress rejected kind: {kind}; category: {error}");
}

#[cfg(test)]
mod tests {

    use winwincode_domain::{ExecutionSequence, SchemaVersion};
    use winwincode_execution_port::generated::{
        WorkerHeartbeatAckMessage, WorkerHeartbeatAckMessageKind, WorkerHeartbeatAckMessageStatus,
    };

    use super::*;

    fn queue_message(
        seed: usize,
        worker: &WorkerId,
        instance: &WorkerInstanceId,
        padding: usize,
    ) -> ExecutionPortMessage {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/contracts/execution-port.valid.json"
        ))
        .unwrap();
        let mut value = fixture["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|value| value["kind"] == "worker.heartbeat_ack")
            .unwrap()
            .clone();
        value["messageId"] = serde_json::json!(format!("xmsg_{seed:026}"));
        value["workerId"] = serde_json::json!(worker.0);
        value["workerInstanceId"] = serde_json::json!(instance.0);
        if padding > 0 {
            value["error"] = serde_json::json!({"code":"INFRASTRUCTURE_ERROR","message":"z".repeat(padding),"retryable":true});
        }
        serde_json::from_value(value).unwrap()
    }

    fn cancel_message(seed: usize) -> ExecutionPortMessage {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/contracts/execution-port.valid.json"
        ))
        .unwrap();
        let mut value = fixture["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|value| value["kind"] == "job.cancel")
            .unwrap()
            .clone();
        value["messageId"] = serde_json::json!(format!("xmsg_{seed:026}"));
        serde_json::from_value(value).unwrap()
    }

    fn lease_message(kind: &str, seed: usize, expires_at: &str) -> ExecutionPortMessage {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/contracts/execution-port.valid.json"
        ))
        .unwrap();
        let mut value = fixture["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|value| value["kind"] == kind)
            .unwrap()
            .clone();
        value["messageId"] = serde_json::json!(format!("xmsg_{seed:026}"));
        value["lease"]["issuedAt"] = serde_json::json!("2026-10-15T00:00:00.000Z");
        value["lease"]["expiresAt"] = serde_json::json!(expires_at);
        serde_json::from_value(value).unwrap()
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn queued_execution_controls_expire_without_rewriting_the_original_frames() {
        let mut nonce = [0_u8; 8];
        getrandom::fill(&mut nonce).unwrap();
        let directory = std::env::temp_dir().join(format!(
            "wwc-remote-lease-queue-{}-{}",
            std::process::id(),
            u64::from_ne_bytes(nonce)
        ));
        let mut storage = SqliteStorage::open(&directory).unwrap();
        storage.execution_registry().unwrap();
        let database = rusqlite::Connection::open(storage.database_path()).unwrap();
        let dispatch = lease_message("job.dispatch", 801, "2026-10-15T00:00:10.000Z");
        let renewal = lease_message("lease.renew", 802, "2026-10-15T00:00:20.000Z");
        let ExecutionPortMessage::JobDispatchMessage(message) = &dispatch else {
            unreachable!()
        };
        let initial = ExecutionLeaseRecord {
            job_id: message.lease.job_id.clone(),
            lease_id: message.lease.lease_id.clone(),
            payload_digest: message.job.payload_digest.clone(),
            worker_id: message.lease.worker_id.clone(),
            worker_instance_id: message.lease.worker_instance_id.clone(),
            attempt: u64::try_from(message.lease.attempt).unwrap(),
            fencing_token: message.lease.fencing_token.clone(),
            issued_at: message.lease.issued_at.clone(),
            expires_at: message.lease.expires_at.clone(),
        };
        let mut current = initial.clone();
        current.expires_at = Instant("2026-10-15T00:00:20.000Z".into());
        database
            .execute(
                "INSERT INTO execution_leases VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                rusqlite::params![
                    current.job_id.0,
                    current.lease_id.0,
                    current.payload_digest.0,
                    current.worker_id.0,
                    current.worker_instance_id.0,
                    i64::try_from(current.attempt).unwrap(),
                    current.fencing_token.0,
                    current.issued_at.0,
                    current.expires_at.0
                ],
            )
            .unwrap();
        for (seed, period) in [&initial, &current].into_iter().enumerate() {
            let receipt = serde_json::json!({"status":"accepted","lease":period,"replayed":false});
            database.execute(
                "INSERT INTO execution_lease_request_receipts VALUES ('claim',?1,?2,'fixture',?3)",
                rusqlite::params![period.job_id.0,format!("req_{seed:026}"),serde_json::to_string(&receipt).unwrap()],
            ).unwrap();
        }
        let queue = RemoteDeliveryQueue::default();
        let worker = initial.worker_id.clone();
        let instance = initial.worker_instance_id.clone();
        queue.enqueue_for(&worker, &instance, dispatch).unwrap();
        queue
            .enqueue_for(&worker, &instance, renewal.clone())
            .unwrap();
        let original = queue.snapshot(&worker, &instance).unwrap();
        let mut stale = renewal.clone();
        let ExecutionPortMessage::LeaseRenewMessage(message) = &mut stale else {
            unreachable!()
        };
        message.message_id = ExecutionMessageId(format!("xmsg_{:026}", 803));
        message.lease.fencing_token.0 = "41".into();
        queue.enqueue_for(&worker, &instance, stale).unwrap();
        let mut unknown_period = renewal;
        let ExecutionPortMessage::LeaseRenewMessage(message) = &mut unknown_period else {
            unreachable!()
        };
        message.message_id = ExecutionMessageId(format!("xmsg_{:026}", 804));
        message.lease.expires_at.0 = "2026-10-15T00:00:19.000Z".into();
        queue
            .enqueue_for(&worker, &instance, unknown_period)
            .unwrap();
        queue
            .retain_live_leases(
                &storage.execution_registry().unwrap(),
                &Instant("2026-10-15T00:00:05.000Z".into()),
            )
            .unwrap();
        assert_eq!(queue.snapshot(&worker, &instance).unwrap(), original);
        queue
            .retain_live_leases(
                &storage.execution_registry().unwrap(),
                &Instant("2026-10-15T00:00:10.000Z".into()),
            )
            .unwrap();
        assert_eq!(queue.snapshot(&worker, &instance).unwrap(), original[1..]);
        queue
            .retain_live_leases(
                &storage.execution_registry().unwrap(),
                &Instant("2026-10-15T00:00:20.000Z".into()),
            )
            .unwrap();
        assert!(queue.snapshot(&worker, &instance).unwrap().is_empty());
        drop(database);
        Box::new(storage).close().unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn terminal_instances_reclaim_pending_and_confirmed_without_dropping_live_instances() {
        let queue = RemoteDeliveryQueue::default();
        let worker = WorkerId("wrk_00000000000000000000000001".into());
        let active = WorkerInstanceId("wki_00000000000000000000000001".into());
        let terminal = WorkerInstanceId("wki_00000000000000000000000002".into());
        for instance in [&active, &terminal] {
            queue
                .enqueue_for(&worker, instance, cancel_message(701))
                .unwrap();
            let first = queue.snapshot(&worker, instance).unwrap();
            queue
                .acknowledge(&worker, instance, &[first[0].delivery_id.clone()])
                .unwrap();
            queue
                .enqueue_for(&worker, instance, cancel_message(702))
                .unwrap();
        }
        let live = std::collections::HashSet::from([(worker.clone(), active.clone())]);
        queue.retain_active_instances(&live).unwrap();
        assert_eq!(queue.snapshot(&worker, &active).unwrap().len(), 1);
        assert!(queue.snapshot(&worker, &terminal).unwrap().is_empty());
        let confirmed = queue.confirmed.lock().unwrap();
        assert!(confirmed.contains_key(&(worker.clone(), active.clone())));
        assert!(!confirmed.contains_key(&(worker, terminal)));
    }

    #[test]
    fn confirmed_cancel_is_not_redelivered_to_the_same_instance() {
        let queue = RemoteDeliveryQueue::default();
        let worker = WorkerId("wrk_00000000000000000000000001".into());
        let instance = WorkerInstanceId("wki_00000000000000000000000001".into());
        let next = WorkerInstanceId("wki_00000000000000000000000002".into());
        queue
            .enqueue_for(&worker, &instance, cancel_message(7))
            .unwrap();
        let page = queue.snapshot(&worker, &instance).unwrap();
        assert_eq!(page.len(), 1);
        queue
            .acknowledge(&worker, &instance, &[page[0].delivery_id.clone()])
            .unwrap();
        for _ in 0..3 {
            queue
                .enqueue_for(&worker, &instance, cancel_message(7))
                .unwrap();
        }
        assert!(
            queue.snapshot(&worker, &instance).unwrap().is_empty(),
            "a confirmed cancel must not keep the Worker inbox busy"
        );
        queue
            .enqueue_for(&worker, &instance, cancel_message(8))
            .unwrap();
        assert_eq!(queue.snapshot(&worker, &instance).unwrap().len(), 1);
        queue
            .enqueue_for(&worker, &next, cancel_message(7))
            .unwrap();
        assert_eq!(queue.snapshot(&worker, &next).unwrap().len(), 1);
        queue
            .enqueue_for(&worker, &instance, queue_message(9, &worker, &instance, 0))
            .unwrap();
        let ids = queue
            .snapshot(&worker, &instance)
            .unwrap()
            .into_iter()
            .map(|delivery| delivery.delivery_id)
            .collect::<Vec<_>>();
        queue.acknowledge(&worker, &instance, &ids).unwrap();
        queue
            .enqueue_for(&worker, &instance, queue_message(9, &worker, &instance, 0))
            .unwrap();
        assert_eq!(queue.snapshot(&worker, &instance).unwrap().len(), 1);
    }

    #[test]
    fn heartbeat_negative_and_positive_receipts_have_separate_delivery_lifetimes() {
        use winwincode_execution_port::generated::{ExecutionPortError, ExecutionPortErrorCode};
        let queue = RemoteDeliveryQueue::default();
        let worker = WorkerId("wrk_00000000000000000000000001".into());
        let instance = WorkerInstanceId("wki_00000000000000000000000001".into());
        let ExecutionPortMessage::WorkerHeartbeatAckMessage(mut gap) =
            queue_message(1, &worker, &instance, 0)
        else {
            unreachable!()
        };
        gap.status = WorkerHeartbeatAckMessageStatus::RejectedWorkerInstance;
        gap.error = Some(ExecutionPortError {
            code: ExecutionPortErrorCode::SequenceGap,
            message: "retry original sequence".into(),
            retryable: true,
        });
        let mut accepted = gap.clone();
        accepted.error = None;
        accepted.status = WorkerHeartbeatAckMessageStatus::Accepted;
        queue
            .enqueue_for(
                &worker,
                &instance,
                ExecutionPortMessage::WorkerHeartbeatAckMessage(gap.clone()),
            )
            .unwrap();
        queue
            .enqueue_for(
                &worker,
                &instance,
                ExecutionPortMessage::WorkerHeartbeatAckMessage(accepted.clone()),
            )
            .unwrap();
        let page = queue.snapshot(&worker, &instance).unwrap();
        assert_eq!(
            page.len(),
            2,
            "a retryable negative reply cannot hide later acceptance"
        );
        assert_ne!(page[0].delivery_id, page[1].delivery_id);
        queue
            .enqueue_for(
                &worker,
                &instance,
                ExecutionPortMessage::WorkerHeartbeatAckMessage(gap),
            )
            .unwrap();
        accepted.status = WorkerHeartbeatAckMessageStatus::Duplicate;
        queue
            .enqueue_for(
                &worker,
                &instance,
                ExecutionPortMessage::WorkerHeartbeatAckMessage(accepted),
            )
            .unwrap();
        assert_eq!(queue.snapshot(&worker, &instance).unwrap().len(), 2);
        queue
            .pending
            .lock()
            .unwrap()
            .retain(|(_, _, id, _)| id != &page[0].delivery_id);
        let remaining = queue.snapshot(&worker, &instance).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].delivery_id, page[1].delivery_id);
        for delivery in remaining {
            let frame = RemoteTransportAdapter::<NoopCore>::decode(&delivery.frame).unwrap();
            assert_eq!(
                execution_message_id(frame.message()).unwrap(),
                delivery.delivery_id
            );
        }
    }

    #[test]
    fn byte_limited_pages_can_drain_a_backpressured_worker() {
        let queue = RemoteDeliveryQueue::default();
        let worker = WorkerId("wrk_00000000000000000000000001".into());
        let instance = WorkerInstanceId("wki_00000000000000000000000001".into());
        for seed in 0..MAX_PENDING_REMOTE_DELIVERIES {
            queue
                .enqueue_for(
                    &worker,
                    &instance,
                    queue_message(seed, &worker, &instance, 32 * 1024),
                )
                .unwrap();
        }
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/contracts/execution-port.valid.json"
        ))
        .unwrap();
        let mut heartbeat = fixture["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["kind"] == "worker.heartbeat")
            .unwrap()
            .clone();
        heartbeat["workerId"] = serde_json::json!(worker.0);
        heartbeat["workerInstanceId"] = serde_json::json!(instance.0);
        let frame = RemoteTransportAdapter::<NoopCore>::encode(
            &TypedFrame::new(
                FrameDirection::WorkerToControlPlane,
                serde_json::from_value(heartbeat).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
        let mut delivered = 0;
        loop {
            let page = queue.snapshot(&worker, &instance).unwrap();
            if page.is_empty() {
                break;
            }
            assert!(page.len() < 128);
            let ids = page.into_iter().map(|d| d.delivery_id).collect::<Vec<_>>();
            delivered += ids.len();
            let request =
                RemoteExchangeRequest::new(worker.clone(), instance.clone(), ids, frame.clone())
                    .unwrap();
            assert!(
                queue.prepare_ingress(&request).is_ok(),
                "ACK and pull must remain available under backpressure"
            );
            assert!(delivered <= 256);
        }
        assert_eq!(delivered, 256);
    }

    #[test]
    fn dead_worker_backlog_cannot_block_another_worker_and_pages_keep_the_suffix() {
        let queue = RemoteDeliveryQueue::default();
        let dead = WorkerId("wrk_00000000000000000000000001".into());
        let live = WorkerId("wrk_00000000000000000000000002".into());
        let instance = WorkerInstanceId("wki_00000000000000000000000001".into());
        for seed in 0..MAX_PENDING_REMOTE_DELIVERIES {
            queue
                .enqueue_for(&dead, &instance, queue_message(seed, &dead, &instance, 0))
                .unwrap();
        }
        assert!(
            queue
                .enqueue_for(&dead, &instance, queue_message(300, &dead, &instance, 0))
                .is_err()
        );
        for seed in 400..408 {
            queue
                .enqueue_for(
                    &live,
                    &instance,
                    queue_message(seed, &live, &instance, 200 * 1024),
                )
                .unwrap();
        }
        let page = queue.snapshot(&live, &instance).unwrap();
        assert!(!page.is_empty() && page.len() < 8);
        let encoded = RemoteExchangeResponse::new(page.clone())
            .unwrap()
            .encode()
            .unwrap();
        assert!(encoded.len() <= winwincode_execution_port::transport::MAX_REMOTE_RESPONSE_BYTES);
        assert_eq!(
            RemoteExchangeResponse::decode(&encoded)
                .unwrap()
                .deliveries(),
            page
        );
        assert_eq!(
            queue.snapshot(&live, &instance).unwrap(),
            page,
            "lost response retains the same page"
        );
        queue
            .acknowledge(
                &live,
                &instance,
                &page
                    .iter()
                    .map(|delivery| delivery.delivery_id.clone())
                    .collect::<Vec<_>>(),
            )
            .unwrap();
        assert!(
            !queue.snapshot(&live, &instance).unwrap().is_empty(),
            "undelivered suffix stays queued"
        );
        assert_eq!(
            queue
                .pending
                .lock()
                .unwrap()
                .iter()
                .filter(|(worker, _, _, _)| worker == &dead)
                .count(),
            256
        );
    }

    #[test]
    fn duplicate_delivery_id_replays_the_first_response() {
        let queue = RemoteDeliveryQueue::default();
        let worker_id = WorkerId("wrk_00000000000000000000000001".to_owned());
        let response = |sent_at: &str, status| {
            ExecutionPortMessage::WorkerHeartbeatAckMessage(WorkerHeartbeatAckMessage {
                error: None,
                heartbeat_sequence: ExecutionSequence(1),
                kind: WorkerHeartbeatAckMessageKind::WorkerHeartbeatAck,
                message_id: ExecutionMessageId("msg_00000000000000000000000001".to_owned()),
                next_heartbeat_within_ms: 1_000,
                schema_version: SchemaVersion::WinwincodeV1,
                sent_at: Instant(sent_at.to_owned()),
                server_time: Instant(sent_at.to_owned()),
                status,
                worker_id: worker_id.clone(),
                worker_instance_id: WorkerInstanceId("wki_00000000000000000000000001".to_owned()),
            })
        };
        queue
            .enqueue_for(
                &worker_id,
                &WorkerInstanceId("wki_00000000000000000000000001".to_owned()),
                response(
                    "2026-09-12T00:00:00.000Z",
                    WorkerHeartbeatAckMessageStatus::Accepted,
                ),
            )
            .expect("first response queues");
        queue
            .enqueue_for(
                &worker_id,
                &WorkerInstanceId("wki_00000000000000000000000001".to_owned()),
                response(
                    "2026-09-12T00:00:01.000Z",
                    WorkerHeartbeatAckMessageStatus::Duplicate,
                ),
            )
            .expect("duplicate response reuses the pending delivery");

        assert_eq!(
            queue
                .snapshot(
                    &worker_id,
                    &WorkerInstanceId("wki_00000000000000000000000001".to_owned())
                )
                .expect("queue snapshot")
                .len(),
            1
        );
        let old = WorkerInstanceId("wki_00000000000000000000000001".into());
        let next = WorkerInstanceId("wki_00000000000000000000000002".into());
        assert!(queue.snapshot(&worker_id, &next).unwrap().is_empty());
        queue
            .acknowledge(
                &worker_id,
                &next,
                &[ExecutionMessageId("msg_00000000000000000000000001".into())],
            )
            .unwrap();
        assert_eq!(queue.snapshot(&worker_id, &old).unwrap().len(), 1);
        queue.retire_other_instances(&worker_id, &next).unwrap();
        assert!(queue.snapshot(&worker_id, &old).unwrap().is_empty());
    }
}
