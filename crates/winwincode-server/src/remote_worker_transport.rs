// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::Mutex;

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
use winwincode_storage::{SqliteStorage, WorkerPoolId, WorkerRegistryScope};

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

    fn snapshot(
        &self,
        worker_id: &WorkerId,
        instance_id: &WorkerInstanceId,
    ) -> Result<Vec<RemoteExchangeDelivery>, RemoteWorkerTransportError> {
        let pending = self.pending.lock().map_err(|_| {
            RemoteWorkerTransportError::new("remote Worker delivery queue is unavailable")
        })?;
        let mut deliveries = Vec::new();
        let mut bytes = RemoteExchangeResponse::with_acceptance(Vec::new(), false)
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
}

impl<Core> ProductionRemoteWorkerExchange<Core> {
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
        }
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
        let deliveries = self
            .queue
            .snapshot(request.worker_id(), request.worker_instance_id())?;
        let response = if request.supports_acceptance_receipt() {
            RemoteExchangeResponse::with_acceptance(deliveries, frame_accepted)
        } else {
            RemoteExchangeResponse::new(deliveries)
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
