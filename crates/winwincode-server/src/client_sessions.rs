// SPDX-License-Identifier: Apache-2.0

//! User-facing Worker launch flow over the durable `WorkerLaunchGrant`
//! ledger (plan 14.2-14.3, 17.2, contract `client-control-port-v1.md`
//! `client.worker.launch` / `client.worker.launch_ack` / `client.worker.stop`).
//!
//! `POST /api/v1/sessions` is the launch entry the signed-in occupancy holder
//! uses to start one `WorkerSession` on the Client they occupy. The flow
//! validates the durable preconditions (`WorkerLaunchGrantService::issue`:
//! the caller is the lease holder, the lease is `occupied` or `draining`,
//! the binding belongs to the leased Client and is visible to the holder,
//! and a worker-session slot is free), mints the worker identities and a
//! 32-byte short-lived worker credential through the credential lifecycle
//! service (only the `sha256:` digest is persisted; the durable credential
//! row is what revoke, rotate, and expiry resolve against), enqueues the
//! `client.worker.launch` downlink frame with every
//! `C + L` field into the durable outbox, and waits a bounded, configurable
//! interval for the Device Client's `client.worker.launch_ack` to be settled
//! by the client exchange (`settle_launch_ack`). An accepted acknowledgement
//! consumes the grant exactly once; a rejection keeps it `issued` with the
//! reason in the launch audit trail.
//!
//! `worker_stop_message` is the shared `client.worker.stop` construction
//! helper: the supervisor and release flows stamp the same occupancy
//! fencing context so the device rejects any stop carrying a stale token.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;
use serde_json::json;
use winwincode_client_port::domain::ClientWorkerStopReason;
use winwincode_client_port::domain::WorkerLaunchGrant;
use winwincode_client_port::domain::WorkerLaunchGrantState as WireGrantState;
use winwincode_client_port::exchange::DEFAULT_MAX_FRAME_BYTES;
use winwincode_client_port::exchange::FrameCodec;
use winwincode_client_port::managed_app::{
    MANAGED_APP_RUN_CONFIG_SCHEMA_VERSION, ManagedAppCommand, ManagedAppOperation,
    ManagedAppRunConfig, ManagedAppState, ManagedAppStatus,
};
use winwincode_client_port::messages::CLIENT_CONTROL_PORT_SCHEMA_VERSION;
use winwincode_client_port::messages::CommandContext;
use winwincode_client_port::messages::OccupancyCommandContext;
use winwincode_client_port::messages::ServerManagedAppCommandPayload;
use winwincode_client_port::messages::ServerToClientEnvelope;
use winwincode_client_port::messages::ServerToClientMessage;
use winwincode_client_port::messages::ServerWorkerLaunchPayload;
use winwincode_client_port::messages::ServerWorkerStopPayload;
use winwincode_control_plane::ClientOccupancyService;
use winwincode_control_plane::ClientRegistryService;
use winwincode_control_plane::LaunchGrantState;
use winwincode_control_plane::OccupancyLeaseState;
use winwincode_control_plane::ProductSessionState;
use winwincode_control_plane::ProductStateStorage;
use winwincode_control_plane::WorkerLaunchGrantService;
use winwincode_control_plane::WorkerLaunchGrantServiceErrorKind;
use winwincode_control_plane::dispatch_work_run_to_device_worker;
use winwincode_domain::{Instant, WorkRunId};
use winwincode_execution_port::generated::ExecutionJob;
use winwincode_storage::ClientDownlinkAppend;
use winwincode_storage::ClientNodeRecord;
use winwincode_storage::ClientPresenceState;
use winwincode_storage::DeviceExecutionBindingState;
use winwincode_storage::ExecutionJobState;
use winwincode_storage::SqliteStorage;
use winwincode_storage::WorkerSlotState;

use crate::client_exchange::{
    ClientExchangeApplication, ClientExchangeConfig, ClientExchangePort, WorkerCredentialDelivery,
};
use crate::client_occupancy::client_mirror_revision_view;
use crate::client_occupancy::offset_instant;
use crate::worker_session_credentials::WorkerSessionCredentialService;
use crate::worker_session_credentials::issue_credential_material;

/// Schema version of the public browser-facing launch surface.
const SUPPORTED_SCHEMA_VERSION: &str = "winwincode/v1";

/// Bounded-wait and credential policy of the launch flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientSessionsConfig {
    /// How long one launch waits for the Device Client launch acknowledgement
    /// before failing (plan 14.3, bounded step 10).
    pub launch_wait: std::time::Duration,
    /// How often the durable grant state is polled while waiting.
    pub poll_interval: std::time::Duration,
    /// Time-to-live of one issued launch grant; an unanswered grant expires
    /// at `issuedAt + ttl` and can no longer be consumed.
    pub grant_ttl: std::time::Duration,
}

impl Default for ClientSessionsConfig {
    fn default() -> Self {
        Self {
            launch_wait: std::time::Duration::from_secs(30),
            poll_interval: std::time::Duration::from_millis(200),
            grant_ttl: std::time::Duration::from_mins(2),
        }
    }
}

/// Stable failure categories of the launch flow boundary. Each category maps
/// to exactly one wire error code of the central launch error-code table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClientSessionsErrorKind {
    /// The request body violated the launch contract.
    InvalidRequest,
    /// The public Client ID does not name a launchable Client.
    ClientNotFound,
    /// The Client is not reachable (offline or degraded).
    ClientOffline,
    /// The Client is locked by a local operator.
    ClientLocked,
    /// The occupancy lease belongs to another user.
    NotHolder,
    /// The Client has no usable occupancy (none, unconfirmed, or pending
    /// recovery).
    OccupancyRequired,
    /// The binding is unknown, foreign, or invisible to the holder.
    BindingNotVisible,
    /// The Client has no free worker-session slot.
    CapacityExhausted,
    /// The grant expired before the device answered.
    GrantExpired,
    /// The Device Client rejected the launch, or the grant was revoked
    /// while waiting.
    LaunchRejected,
    /// The Device Client did not answer within the bounded wait; the grant
    /// stays `issued` until its expiry.
    LaunchAckTimeout,
    /// The ProductSession or its prior Device Worker cannot be started in the
    /// current durable state. Callers must recover/stop the prior worker or
    /// reopen a live conversation; this is never a fake success.
    SessionNotLaunchable,
    /// Durable state or storage failed; nothing was decided.
    Unavailable,
}

/// Secret-free launch flow failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientSessionsError {
    kind: ClientSessionsErrorKind,
    message: String,
}

impl ClientSessionsError {
    #[must_use]
    pub const fn kind(&self) -> ClientSessionsErrorKind {
        self.kind
    }

    fn new(kind: ClientSessionsErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    fn invalid_request() -> Self {
        Self::new(
            ClientSessionsErrorKind::InvalidRequest,
            "launch request must carry a 9-12 digit clientId, a repositoryBindingId, and either productSession or workRunId",
        )
    }

    fn unavailable() -> Self {
        Self::new(
            ClientSessionsErrorKind::Unavailable,
            "client session launch service is unavailable",
        )
    }
}

impl fmt::Display for ClientSessionsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ClientSessionsError {}

/// What one validated launch prepared before the bounded wait.
struct PreparedLaunch {
    node: ClientNodeRecord,
    occupancy_lease_id: String,
    occupancy_fencing_token: u64,
    grant: winwincode_storage::WorkerLaunchGrantRecord,
    product_session_id: String,
    work_run_id: Option<String>,
}

/// The outcome of one poll of a pending launch grant.
enum PollOutcome {
    /// The grant is still `issued`; keep waiting.
    Pending,
    /// The grant was consumed; the flow returns the `201` body.
    Consumed,
    /// The flow failed with the mapped domain error.
    Failed(ClientSessionsError),
}

/// The signed-in user's Worker launch surface over the Server's one
/// product-state database directory. Like the connect and occupancy flows,
/// every operation opens and closes its own storage connection so concurrent
/// flows never share state in memory and the bounded wait holds no database
/// lock.
#[derive(Clone)]
pub struct ClientSessionsApplication {
    data_directory: PathBuf,
    config: ClientSessionsConfig,
    client_exchange: Arc<dyn ClientExchangePort>,
}

impl ClientSessionsApplication {
    pub(crate) fn resume_delivery_launches(&self) -> Result<(), ClientSessionsError> {
        let jobs = self
            .open_storage()?
            .repository_scheduler()
            .map_err(|_| ClientSessionsError::unavailable())?
            .pending_device_launches()
            .map_err(|_| ClientSessionsError::unavailable())?;
        for payload in jobs {
            let job: ExecutionJob =
                serde_json::from_slice(&payload).map_err(|_| ClientSessionsError::unavailable())?;
            let winwincode_execution_port::generated::ExecutionScope::WorkRunExecutionScope(scope) =
                &job.scope
            else {
                continue;
            };
            let Some(target) = job
                .work_input
                .as_ref()
                .and_then(|input| input.device_target.as_ref())
            else {
                continue;
            };
            // Each pass advances every queued role once; an offline Device does not delay other jobs.
            let prepared = self.prepare(
                &target.user_id,
                &json!({
                    "schemaVersion": SUPPORTED_SCHEMA_VERSION,
                    "clientId": target.client_id,
                    "repositoryBindingId": target.repository_binding_id,
                    "workRunId": scope.work_run_id,
                }),
            );
            if let Ok(prepared) = prepared
                && matches!(
                    self.poll(&prepared.grant.worker_launch_grant_id),
                    Ok(PollOutcome::Consumed)
                )
            {
                self.route_work_run(&target.user_id, &prepared)?;
            }
        }
        Ok(())
    }
    /// Composes the launch application over one product-state directory.
    ///
    /// # Errors
    ///
    /// Fails when the configuration violates its bounds.
    pub fn open(
        data_directory: impl Into<PathBuf>,
        config: &ClientSessionsConfig,
    ) -> Result<Self, ClientSessionsError> {
        let data_directory = data_directory.into();
        let client_exchange = Arc::new(
            ClientExchangeApplication::open(&data_directory, &ClientExchangeConfig::default())
                .map_err(|_| ClientSessionsError::unavailable())?,
        );
        Self::open_with_exchange(data_directory, config, client_exchange)
    }

    /// Composes the launch application with the exact Device exchange used
    /// by the running Server, so ephemeral Worker credentials travel on the
    /// same authenticated response as their launch frame.
    ///
    /// # Errors
    ///
    /// Fails when the configuration violates its bounds.
    pub fn open_with_exchange(
        data_directory: impl Into<PathBuf>,
        config: &ClientSessionsConfig,
        client_exchange: Arc<dyn ClientExchangePort>,
    ) -> Result<Self, ClientSessionsError> {
        if config.launch_wait.is_zero()
            || config.poll_interval.is_zero()
            || config.grant_ttl.is_zero()
        {
            return Err(ClientSessionsError::new(
                ClientSessionsErrorKind::InvalidRequest,
                "client session configuration bounds must be positive",
            ));
        }
        Ok(Self {
            data_directory: data_directory.into(),
            config: config.clone(),
            client_exchange,
        })
    }

    /// Runs the full launch flow (plan 14.3, steps 3-5 with the bounded
    /// device acknowledgement) and resolves to the `201` session body.
    ///
    /// # Errors
    ///
    /// Returns the stable launch failure categories; `LaunchAckTimeout`
    /// leaves the grant `issued` (it expires at its deadline), and
    /// `LaunchRejected` covers a Device Client that answered with a
    /// rejection.
    pub async fn launch(
        &self,
        user_id: &str,
        request: &Value,
    ) -> Result<Value, ClientSessionsError> {
        let prepared = self.prepare(user_id, request)?;
        let grant_id = prepared.grant.worker_launch_grant_id.clone();
        let deadline = tokio::time::Instant::now() + self.config.launch_wait;
        loop {
            match self.poll(&grant_id)? {
                PollOutcome::Pending => {}
                PollOutcome::Consumed => {
                    self.route_work_run(user_id, &prepared)?;
                    return Ok(session_body(&prepared));
                }
                PollOutcome::Failed(error) => return Err(error),
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(ClientSessionsError::new(
                    ClientSessionsErrorKind::LaunchAckTimeout,
                    "the device did not acknowledge the worker launch in time",
                ));
            }
            tokio::time::sleep(self.config.poll_interval).await;
        }
    }

    /// Schedules one Device-owned managed application command and returns the
    /// latest status projection known from the Client exchange.
    ///
    /// # Errors
    ///
    /// Returns a stable client-session error when the run, device, occupancy,
    /// or durable managed-app authority is unavailable or invalid.
    pub fn manage_managed_app(
        &self,
        user_id: &str,
        public_client_id: &str,
        request: &Value,
    ) -> Result<Value, ClientSessionsError> {
        let mut command: ManagedAppCommand = serde_json::from_value(request.clone())
            .map_err(|_| ClientSessionsError::invalid_request())?;
        let mut storage = self.open_storage()?;
        let node = Self::lookup_node(&mut storage, public_client_id)?;
        let authoritative =
            load_authoritative_managed_app_config(&storage, &command.run_id, command.operation)?;
        // The browser can select only the operation and run identity. The
        // Server attaches the durable executable config immediately before
        // enqueueing so argv, cwd and env never cross the public query or
        // command request boundary.
        if matches!(
            command.operation,
            ManagedAppOperation::Start | ManagedAppOperation::Restart
        ) {
            if command.config.is_some() {
                return Err(ClientSessionsError::invalid_request());
            }
            command.config = Some(authoritative.clone());
        }
        validate_managed_app_command_against_authority(&command, &authoritative)?;
        enqueue_managed_app_command(
            &self.data_directory,
            &mut storage,
            &node,
            user_id,
            command.clone(),
            &now_instant(),
        )?;
        let status = self
            .client_exchange
            .managed_app_status(&node.client_node_id, &command.run_id);
        Ok(managed_app_response(&command, status.as_ref()))
    }

    /// Validates the request and durable preconditions, issues the launch
    /// grant, and enqueues the `client.worker.launch` downlink frame.
    #[allow(clippy::too_many_lines)]
    fn prepare(
        &self,
        user_id: &str,
        request: &Value,
    ) -> Result<PreparedLaunch, ClientSessionsError> {
        let Some(fields) = request.as_object() else {
            return Err(ClientSessionsError::invalid_request());
        };
        if fields.len() != 4
            || fields.get("schemaVersion").and_then(Value::as_str) != Some(SUPPORTED_SCHEMA_VERSION)
        {
            return Err(ClientSessionsError::invalid_request());
        }
        let public_client_id = required_client_id(fields.get("clientId"))?;
        let repository_binding_id = fields
            .get("repositoryBindingId")
            .and_then(Value::as_str)
            .ok_or_else(ClientSessionsError::invalid_request)?
            .to_owned();

        let mut storage = self.open_storage()?;
        let node = Self::lookup_node(&mut storage, &public_client_id)?;
        let now = now_instant();

        // Occupancy: the caller must hold the node's one active lease and it
        // must be device-confirmed (`occupied` or `draining`).
        let lease = {
            let mut occupancy = ClientOccupancyService::new(&mut storage);
            occupancy
                .active_lease_for_node(&node.client_node_id)
                .map_err(|_| ClientSessionsError::unavailable())?
        };
        let Some(lease) = lease else {
            return Err(ClientSessionsError::new(
                ClientSessionsErrorKind::OccupancyRequired,
                "the client is not occupied; claim occupancy before launching",
            ));
        };
        if lease.holder_user_id != user_id {
            return Err(ClientSessionsError::new(
                ClientSessionsErrorKind::NotHolder,
                "only the occupancy holder may launch a worker session",
            ));
        }
        if !matches!(
            lease.state,
            OccupancyLeaseState::Occupied | OccupancyLeaseState::Draining
        ) {
            return Err(ClientSessionsError::new(
                ClientSessionsErrorKind::OccupancyRequired,
                "the occupancy is not confirmed by the device",
            ));
        }

        // Worker identities and the short-lived credential (32 random bytes
        // through the credential lifecycle service; only the digest is
        // persisted).
        let work_run_id = fields
            .get("workRunId")
            .and_then(Value::as_str)
            .map(|id| WorkRunId(id.to_owned()));
        if fields.contains_key("workRunId") && work_run_id.is_none() {
            return Err(ClientSessionsError::invalid_request());
        }
        let product_session_id = if let Some(work_run_id) = &work_run_id {
            let record = storage
                .load_active_execution_job_record_for_work_run(work_run_id)
                .map_err(|_| ClientSessionsError::unavailable())?
                .ok_or_else(|| {
                    ClientSessionsError::new(
                        ClientSessionsErrorKind::InvalidRequest,
                        "workRunId does not name an active execution job",
                    )
                })?;
            let job: ExecutionJob = serde_json::from_slice(&record.dispatch_payload)
                .map_err(|_| ClientSessionsError::unavailable())?;
            if let Some(target) = job
                .work_input
                .as_ref()
                .and_then(|input| input.device_target.as_ref())
                && (target.user_id != user_id
                    || target.client_node_id != node.client_node_id
                    || target.client_id != public_client_id
                    || target.repository_binding_id != repository_binding_id)
            {
                return Err(ClientSessionsError::invalid_request());
            }
            record.scope.product_session_id.0
        } else {
            let session = fields
                .get("productSession")
                .and_then(Value::as_object)
                .filter(|value| value.len() == 2)
                .ok_or_else(ClientSessionsError::invalid_request)?;
            let id: winwincode_domain::ProductSessionId = serde_json::from_value(
                session
                    .get("id")
                    .cloned()
                    .ok_or_else(ClientSessionsError::invalid_request)?,
            )
            .map_err(|_| ClientSessionsError::invalid_request())?;
            let scope: winwincode_domain::RepositoryScope = serde_json::from_value(
                session
                    .get("scope")
                    .cloned()
                    .ok_or_else(ClientSessionsError::invalid_request)?,
            )
            .map_err(|_| ClientSessionsError::invalid_request())?;
            let scope_key = crate::device_providers::repository_scope_key(&scope)
                .map_err(|_| ClientSessionsError::invalid_request())?;
            let record = winwincode_control_plane::ProductSessionService::new(&mut storage)
                .get(&scope_key, &id)
                .map_err(|_| ClientSessionsError::unavailable())?
                .ok_or_else(ClientSessionsError::invalid_request)?;
            if !matches!(record.owner_actor(), winwincode_storage::PublicEventActor::User { id } if id.0 == user_id)
            {
                return Err(ClientSessionsError::invalid_request());
            }
            if matches!(
                record.session().state(),
                ProductSessionState::Cancelled | ProductSessionState::Closed
            ) {
                return Err(ClientSessionsError::new(
                    ClientSessionsErrorKind::SessionNotLaunchable,
                    "the ProductSession is cancelled or closed; reopen a live conversation before launching a Device Worker",
                ));
            }
            crate::device_providers::validate_session_route(
                &mut storage,
                &scope,
                &id,
                &node.client_node_id,
            )
            .map_err(|_| ClientSessionsError::invalid_request())?;
            id.0
        };
        if let Some(grant) = WorkerLaunchGrantService::new(&mut storage)
            .newest_grant_for_product_session(&product_session_id)
            .map_err(|_| ClientSessionsError::unavailable())?
        {
            if grant.client_node_id != node.client_node_id
                || grant.repository_binding_id != repository_binding_id
                || grant.holder_user_id != user_id
            {
                return Err(ClientSessionsError::invalid_request());
            }
            let binding = storage
                .device_execution_binding_ledger()
                .map_err(|_| ClientSessionsError::unavailable())?
                .snapshot(&grant.worker_session_id)
                .map_err(|_| ClientSessionsError::unavailable())?;
            let slot = storage
                .worker_session_slots()
                .map_err(|_| ClientSessionsError::unavailable())?
                .load(&winwincode_domain::WorkerSessionId(
                    grant.worker_session_id.clone(),
                ))
                .map_err(|_| ClientSessionsError::unavailable())?;
            let occupancy_matches = grant.occupancy_lease_id == lease.occupancy_lease_id
                && grant.occupancy_fencing_token == lease.fencing_token;
            let binding_state = binding.as_ref().map(|record| record.state);
            let slot_state = slot.as_ref().map(|record| record.state);
            // Device-reported release is the strongest evidence the prior
            // Chat worker ended under the current occupancy.
            let device_reported_released = work_run_id.is_none()
                && grant.work_run_id.is_none()
                && binding_state == Some(DeviceExecutionBindingState::Released);
            // A terminal Worker-session slot after a bind means the worker
            // process exited without a release report (abnormal exit).
            let slot_exited = slot_state.is_some_and(|state| {
                matches!(
                    state,
                    WorkerSlotState::Completed
                        | WorkerSlotState::Failed
                        | WorkerSlotState::Cancelled
                        | WorkerSlotState::RecoveryFailed
                )
            });
            // Live-worker evidence for a consumed grant under the current
            // occupancy: the Device has not reported release and the Worker
            // slot is not known to be terminal. A still-running slot is the
            // strongest form; a missing slot still maps to the same grant
            // because the Device accepted the launch under this occupancy.
            let reuse_same_worker = occupancy_matches
                && matches!(
                    grant.state,
                    LaunchGrantState::Issued | LaunchGrantState::Consumed
                )
                && !device_reported_released
                && !slot_exited;
            if reuse_same_worker {
                return Ok(PreparedLaunch {
                    node,
                    occupancy_lease_id: lease.occupancy_lease_id,
                    occupancy_fencing_token: lease.fencing_token,
                    grant,
                    product_session_id,
                    work_run_id: work_run_id.map(|id| id.0),
                });
            }
            // A binding still bound under a prior occupancy stamp cannot be
            // proven released. Restarting under the new lease would escalate
            // occupancy privilege; require recovery/stop first.
            let bound_under_foreign_occupancy =
                binding_state == Some(DeviceExecutionBindingState::Bound) && !occupancy_matches;
            if bound_under_foreign_occupancy {
                return Err(ClientSessionsError::new(
                    ClientSessionsErrorKind::SessionNotLaunchable,
                    "the previous Device Worker is still bound under a prior occupancy; recover or stop it before restarting this conversation",
                ));
            }
            if matches!(grant.state, LaunchGrantState::Issued) && !occupancy_matches {
                return Err(ClientSessionsError::new(
                    ClientSessionsErrorKind::OccupancyRequired,
                    "the occupancy behind the pending launch grant is gone",
                ));
            }
            // Restart-with-evidence: the prior worker is not live (device
            // reported release, abnormal exit, or occupancy rewrite without a
            // still-bound foreign binding). Replace the old credential under
            // the *current* occupancy and issue a fresh grant.
            let mut credentials = WorkerSessionCredentialService::new(&mut storage);
            if credentials
                .status_for_session(&grant.worker_session_id)
                .map_err(|_| ClientSessionsError::unavailable())?
                .is_some()
            {
                credentials
                    .revoke_for_session(
                        &grant.worker_session_id,
                        user_id,
                        Some(if device_reported_released {
                            "released Chat worker replaced"
                        } else {
                            "exited Device Worker replaced with restart evidence"
                        }),
                        &now,
                    )
                    .map_err(|_| ClientSessionsError::unavailable())?;
            }
        }
        let worker_session_id = generate_prefixed_id("wsn_")?;
        let worker_id = generate_prefixed_id("wrk_")?;
        let worker_instance_id = generate_prefixed_id("wki_")?;
        let worker_launch_grant_id = generate_prefixed_id("wlg_")?;
        let credential_material =
            issue_credential_material().map_err(|_| ClientSessionsError::unavailable())?;
        let expires_at = offset_instant(&now, duration_millis(self.config.grant_ttl))
            .ok_or_else(ClientSessionsError::unavailable)?;
        let issuance = winwincode_storage::LaunchGrantIssuance::try_new(
            worker_launch_grant_id.clone(),
            node.client_node_id.clone(),
            node.current_instance_id
                .clone()
                .ok_or_else(ClientSessionsError::unavailable)?,
            user_id,
            lease.occupancy_lease_id.clone(),
            lease.fencing_token,
            repository_binding_id,
            worker_session_id.clone(),
            worker_id.clone(),
            worker_instance_id.clone(),
            credential_material.credential_digest().to_owned(),
            Some(product_session_id.clone()),
            work_run_id.clone(),
            expires_at.clone(),
        )
        .map_err(|_| ClientSessionsError::unavailable())?;
        let grant = {
            let mut grants = WorkerLaunchGrantService::new(&mut storage);
            match grants.issue(&issuance, &now) {
                Ok(grant) => grant,
                Err(error) => return Err(issue_gate_error(error.kind())),
            }
        };

        // The durable credential row is the lifecycle handle for the
        // material just issued: revoke, rotate, expiry, and status of the
        // worker session resolve through it. A failure here fails the launch
        // before any downlink frame exists, so the device never learns of a
        // credential the server could not record; the orphaned grant expires
        // at its deadline.
        {
            let mut credentials = WorkerSessionCredentialService::new(&mut storage);
            credentials
                .issue_for_launch(
                    &worker_session_id,
                    &worker_id,
                    &worker_instance_id,
                    &worker_launch_grant_id,
                    credential_material.credential_digest(),
                    &now,
                )
                .map_err(|_| ClientSessionsError::unavailable())?;
        }

        self.client_exchange
            .publish_worker_credential(WorkerCredentialDelivery {
                client_node_id: node.client_node_id.clone(),
                worker_launch_grant_id: worker_launch_grant_id.clone(),
                worker_session_id: worker_session_id.clone(),
                credential_digest: credential_material.credential_digest().to_owned(),
                worker_credential: credential_material.material().to_owned(),
                expires_at: expires_at.clone(),
            })
            .map_err(|_| ClientSessionsError::unavailable())?;

        // The launch command is computed against the mirror revision the
        // device last confirmed: the device refuses any other stamp.
        let mirror_revision_view =
            client_mirror_revision_view(&self.data_directory, &node.client_node_id)
                .map_err(|_| ClientSessionsError::unavailable())?;
        enqueue_frame(
            &mut storage,
            &node,
            ServerToClientMessage::WorkerLaunch(ServerWorkerLaunchPayload {
                occupancy: occupancy_stamp(
                    mirror_revision_view,
                    &lease.occupancy_lease_id,
                    lease.fencing_token,
                    &format!("idem_launch_{}", grant.worker_launch_grant_id),
                ),
                launch_grant: WorkerLaunchGrant {
                    worker_launch_grant_id: grant.worker_launch_grant_id.clone(),
                    client_node_id: node.client_node_id.clone(),
                    client_instance_id: grant.client_instance_id.clone(),
                    occupancy_lease_id: lease.occupancy_lease_id.clone(),
                    occupancy_fencing_token: lease.fencing_token,
                    repository_binding_id: grant.repository_binding_id.clone(),
                    product_session_id: grant.product_session_id.clone().unwrap_or_default(),
                    work_run_id: grant.work_run_id.as_ref().map(|id| id.0.clone()),
                    worker_session_id: grant.worker_session_id.clone(),
                    worker_id: grant.worker_id.clone(),
                    worker_instance_id: grant.worker_instance_id.clone(),
                    credential_digest: grant.credential_digest.clone(),
                    expires_at: grant.expires_at.0.clone(),
                    state: WireGrantState::Issued,
                    revision: grant.revision,
                },
            }),
            &now,
        )?;

        Ok(PreparedLaunch {
            node,
            occupancy_lease_id: lease.occupancy_lease_id,
            occupancy_fencing_token: lease.fencing_token,
            grant,
            product_session_id,
            work_run_id: work_run_id.map(|id| id.0),
        })
    }

    /// Reads the durable grant state once and drives the flow to its next
    /// transition (plan 14.3 step 10). A rejection never moves the state, so
    /// the launch audit trail carries the verdict: a recorded
    /// `launch_rejected` entry fails the flow immediately instead of
    /// burning the whole bounded wait.
    fn poll(&self, worker_launch_grant_id: &str) -> Result<PollOutcome, ClientSessionsError> {
        let mut storage = self.open_storage()?;
        let mut grants = WorkerLaunchGrantService::new(&mut storage);
        let grant = grants
            .snapshot(worker_launch_grant_id)
            .map_err(|_| ClientSessionsError::unavailable())?;
        let Some(grant) = grant else {
            return Ok(PollOutcome::Failed(ClientSessionsError::unavailable()));
        };
        match grant.state {
            LaunchGrantState::Issued => {
                let rejected = grants
                    .audit_trail(worker_launch_grant_id)
                    .map_err(|_| ClientSessionsError::unavailable())?
                    .into_iter()
                    .any(|entry| entry.action.as_str() == "launch_rejected");
                if rejected {
                    Ok(PollOutcome::Failed(ClientSessionsError::new(
                        ClientSessionsErrorKind::LaunchRejected,
                        "the device rejected the worker launch",
                    )))
                } else {
                    Ok(PollOutcome::Pending)
                }
            }
            LaunchGrantState::Consumed => Ok(PollOutcome::Consumed),
            LaunchGrantState::Revoked => Ok(PollOutcome::Failed(ClientSessionsError::new(
                ClientSessionsErrorKind::LaunchRejected,
                "the launch grant was revoked before the device accepted",
            ))),
            LaunchGrantState::Expired => Ok(PollOutcome::Failed(ClientSessionsError::new(
                ClientSessionsErrorKind::GrantExpired,
                "the launch grant expired before the device accepted",
            ))),
        }
    }

    fn route_work_run(
        &self,
        user_id: &str,
        prepared: &PreparedLaunch,
    ) -> Result<(), ClientSessionsError> {
        let Some(work_run_id) = &prepared.work_run_id else {
            return Ok(());
        };
        let mut storage = self.open_storage()?;
        let dispatch = dispatch_work_run_to_device_worker(
            &mut storage,
            Some(user_id),
            &WorkRunId(work_run_id.clone()),
            &now_instant(),
        )
        .map_err(|_| ClientSessionsError::unavailable())?;
        if dispatch.is_none() {
            return Err(ClientSessionsError::new(
                ClientSessionsErrorKind::LaunchRejected,
                "the WorkRun cannot be routed to the launched device worker",
            ));
        }
        Ok(())
    }

    fn lookup_node(
        storage: &mut SqliteStorage,
        public_client_id: &str,
    ) -> Result<ClientNodeRecord, ClientSessionsError> {
        let mut registry = ClientRegistryService::new(storage);
        let record = registry
            .snapshot_by_public_client_id(public_client_id)
            .map_err(|_| ClientSessionsError::unavailable())?;
        match record {
            None
            | Some(ClientNodeRecord {
                presence_state:
                    ClientPresenceState::PendingEnrollment | ClientPresenceState::Revoked,
                ..
            }) => Err(ClientSessionsError::new(
                ClientSessionsErrorKind::ClientNotFound,
                "no client matches the requested id",
            )),
            Some(node)
                if matches!(
                    node.presence_state,
                    ClientPresenceState::Offline | ClientPresenceState::Degraded
                ) =>
            {
                Err(ClientSessionsError::new(
                    ClientSessionsErrorKind::ClientOffline,
                    "the client is not online",
                ))
            }
            Some(node) if node.presence_state == ClientPresenceState::Locked => {
                Err(ClientSessionsError::new(
                    ClientSessionsErrorKind::ClientLocked,
                    "the client is locked",
                ))
            }
            Some(node) => Ok(node),
        }
    }

    fn open_storage(&self) -> Result<SqliteStorage, ClientSessionsError> {
        SqliteStorage::open(&self.data_directory).map_err(|_| ClientSessionsError::unavailable())
    }
}

/// Maps one issue-gate failure onto the central launch error-code taxonomy.
fn issue_gate_error(kind: WorkerLaunchGrantServiceErrorKind) -> ClientSessionsError {
    match kind {
        WorkerLaunchGrantServiceErrorKind::UnknownClientNode
        | WorkerLaunchGrantServiceErrorKind::UnknownOccupancyLease => ClientSessionsError::new(
            ClientSessionsErrorKind::OccupancyRequired,
            "the occupancy behind the launch is gone",
        ),
        WorkerLaunchGrantServiceErrorKind::PresenceNotOnline => ClientSessionsError::new(
            ClientSessionsErrorKind::ClientOffline,
            "the client is not online",
        ),
        WorkerLaunchGrantServiceErrorKind::ClientLocked => ClientSessionsError::new(
            ClientSessionsErrorKind::ClientLocked,
            "the client is locked",
        ),
        WorkerLaunchGrantServiceErrorKind::NotLeaseHolder => ClientSessionsError::new(
            ClientSessionsErrorKind::NotHolder,
            "only the occupancy holder may launch a worker session",
        ),
        WorkerLaunchGrantServiceErrorKind::OccupancyNotConfirmed => ClientSessionsError::new(
            ClientSessionsErrorKind::OccupancyRequired,
            "the occupancy is not confirmed by the device",
        ),
        WorkerLaunchGrantServiceErrorKind::FencingTokenMismatch => ClientSessionsError::new(
            ClientSessionsErrorKind::OccupancyRequired,
            "the occupancy stamp is stale",
        ),
        WorkerLaunchGrantServiceErrorKind::UnknownRepositoryBinding
        | WorkerLaunchGrantServiceErrorKind::BindingForeignClient
        | WorkerLaunchGrantServiceErrorKind::BindingNotVisible => ClientSessionsError::new(
            ClientSessionsErrorKind::BindingNotVisible,
            "the repository binding is not visible to the holder",
        ),
        WorkerLaunchGrantServiceErrorKind::CapacityExhausted => ClientSessionsError::new(
            ClientSessionsErrorKind::CapacityExhausted,
            "the client has no free worker-session slot",
        ),
        WorkerLaunchGrantServiceErrorKind::LaunchGrantConflict => ClientSessionsError::new(
            ClientSessionsErrorKind::CapacityExhausted,
            "the worker session already carries a live launch",
        ),
        _ => ClientSessionsError::unavailable(),
    }
}

/// Builds the secret-free `201` session body. Credential material travels
/// only through the in-memory Device exchange sidecar.
fn session_body(prepared: &PreparedLaunch) -> Value {
    json!({
        "schemaVersion": SUPPORTED_SCHEMA_VERSION,
        "clientId": prepared.node.public_client_id,
        "workerLaunchGrantId": prepared.grant.worker_launch_grant_id,
        "repositoryBindingId": prepared.grant.repository_binding_id,
        "occupancyLeaseId": prepared.occupancy_lease_id,
        "occupancyFencingToken": prepared.occupancy_fencing_token,
        "workerSessionId": prepared.grant.worker_session_id,
        "workerId": prepared.grant.worker_id,
        "workerInstanceId": prepared.grant.worker_instance_id,
        "productSessionId": prepared.product_session_id,
        "workRunId": prepared.work_run_id,
        "credentialDigest": prepared.grant.credential_digest,
        "expiresAt": prepared.grant.expires_at.0,
    })
}

fn managed_app_response(command: &ManagedAppCommand, status: Option<&ManagedAppStatus>) -> Value {
    let phase = status.map_or(
        match command.operation {
            ManagedAppOperation::Start | ManagedAppOperation::Restart => "starting",
            ManagedAppOperation::Stop => "exited",
            ManagedAppOperation::Query => "idle",
        },
        |value| match value.state {
            ManagedAppState::Starting => "starting",
            ManagedAppState::Healthy => "ready",
            ManagedAppState::Unhealthy | ManagedAppState::Missing => "failed",
            ManagedAppState::Stopped | ManagedAppState::Exited => "exited",
        },
    );
    json!({
        "schemaVersion": MANAGED_APP_RUN_CONFIG_SCHEMA_VERSION,
        "runId": command.run_id,
        "leaseId": command.occupancy_lease_id,
        "phase": phase,
        "startedAt": Value::Null,
        "exitedAt": Value::Null,
        "exitCode": status.as_ref().and_then(|value| value.exit_code),
        "failureReason": (phase == "failed").then_some("设备未能让受管应用就绪"),
    })
}

/// Builds the occupancy fencing stamp every occupancy-backed downlink
/// command carries (contract `client-control-port-v1.md`, `C + L`).
#[must_use]
pub fn occupancy_stamp(
    expected_revision: u64,
    occupancy_lease_id: &str,
    fencing_token: u64,
    idempotency_key: &str,
) -> OccupancyCommandContext {
    OccupancyCommandContext {
        command: CommandContext {
            expected_revision,
            idempotency_key: idempotency_key.to_owned(),
        },
        occupancy_lease_id: occupancy_lease_id.to_owned(),
        occupancy_fencing_token: fencing_token,
    }
}

/// Builds one managed application command only when its command lease and
/// fencing token are exactly the occupancy context selected by the Server.
/// The Device performs the same check against its durable mirror before it
/// starts, stops, restarts, or queries the local process.
///
/// # Errors
///
/// Returns an invalid request for malformed commands or an occupancy error for
/// a stale lease or fencing token.
pub fn managed_app_command_message(
    occupancy: &OccupancyCommandContext,
    command: ManagedAppCommand,
) -> Result<ServerToClientMessage, ClientSessionsError> {
    command
        .validate()
        .map_err(|_| ClientSessionsError::invalid_request())?;
    if command.occupancy_lease_id != occupancy.occupancy_lease_id
        || command.occupancy_fencing_token != occupancy.occupancy_fencing_token
    {
        return Err(ClientSessionsError::new(
            ClientSessionsErrorKind::OccupancyRequired,
            "managed application command has a stale occupancy stamp",
        ));
    }
    Ok(ServerToClientMessage::ManagedAppCommand(
        ServerManagedAppCommandPayload { command },
    ))
}

fn load_authoritative_managed_app_config(
    storage: &SqliteStorage,
    run_id: &str,
    operation: ManagedAppOperation,
) -> Result<ManagedAppRunConfig, ClientSessionsError> {
    let record = storage
        .load_managed_app_run_config(run_id)
        .map_err(|_| ClientSessionsError::unavailable())?
        .ok_or_else(ClientSessionsError::invalid_request)?;
    if record.run_id != run_id {
        return Err(ClientSessionsError::invalid_request());
    }
    let config: ManagedAppRunConfig = serde_json::from_slice(&record.config_json)
        .map_err(|_| ClientSessionsError::unavailable())?;
    config
        .validate()
        .map_err(|_| ClientSessionsError::unavailable())?;
    if config.run_id != record.run_id
        || config.repository_binding_id != record.repository_binding_id
        || i64::from(config.attempt) != record.attempt
    {
        return Err(ClientSessionsError::unavailable());
    }
    validate_config_against_job(storage, &config, operation)?;
    Ok(config)
}

fn validate_config_against_job(
    storage: &SqliteStorage,
    config: &ManagedAppRunConfig,
    operation: ManagedAppOperation,
) -> Result<(), ClientSessionsError> {
    let work_run_id = WorkRunId(config.run_id.clone());
    let source_job_id = winwincode_domain::ExecutionJobId(config.source_id.clone());
    let job_record = storage
        .load_execution_job_record(&source_job_id)
        .map_err(|_| ClientSessionsError::unavailable())?
        .ok_or_else(ClientSessionsError::invalid_request)?;
    if job_record.job_id != source_job_id
        || job_record.work_run_id.as_ref() != Some(&work_run_id)
        || job_record.attempt != u64::from(config.attempt)
    {
        return Err(ClientSessionsError::invalid_request());
    }
    let job: ExecutionJob = serde_json::from_slice(&job_record.dispatch_payload)
        .map_err(|_| ClientSessionsError::unavailable())?;
    if job.job_id != job_record.job_id
        || job.attempt != i64::try_from(job_record.attempt).unwrap_or(-1)
    {
        return Err(ClientSessionsError::unavailable());
    }
    let winwincode_execution_port::generated::ExecutionScope::WorkRunExecutionScope(scope) =
        &job.scope
    else {
        return Err(ClientSessionsError::invalid_request());
    };
    if scope.work_run_id != work_run_id
        || scope.attempt != i64::try_from(job_record.attempt).unwrap_or(-1)
    {
        return Err(ClientSessionsError::invalid_request());
    }
    let Some(target) = job
        .work_input
        .as_ref()
        .and_then(|input| input.device_target.as_ref())
    else {
        return Err(ClientSessionsError::invalid_request());
    };
    if target.repository_binding_id != config.repository_binding_id {
        return Err(ClientSessionsError::invalid_request());
    }
    if config.mode == winwincode_client_port::managed_app::ManagedAppMode::FrozenCandidate
        && config.candidate_commit.as_deref() != Some(job.workspace.checkout_revision.as_str())
    {
        return Err(ClientSessionsError::invalid_request());
    }
    if matches!(
        (operation, config.mode),
        (
            ManagedAppOperation::Stop | ManagedAppOperation::Query,
            winwincode_client_port::managed_app::ManagedAppMode::FrozenCandidate,
        )
    ) {
        let source = winwincode_client_port::preview::PreviewSourceDescriptor {
            source_id: config.source_id.clone(),
            work_run_id: config.run_id.clone(),
            repository_binding_id: config.repository_binding_id.clone(),
            mode: winwincode_client_port::preview::PreviewSourceMode::FrozenCandidate,
            candidate_commit: config.candidate_commit.clone(),
        };
        crate::preview::authorize_current_frozen_candidate(
            storage,
            &job_record,
            &work_run_id,
            &source,
            config,
            &job,
        )
        .map_err(|_| ClientSessionsError::invalid_request())?;
    }
    let allowed = match (operation, config.mode) {
        (ManagedAppOperation::Stop | ManagedAppOperation::Query, _) => true,
        (
            ManagedAppOperation::Start | ManagedAppOperation::Restart,
            winwincode_client_port::managed_app::ManagedAppMode::Live,
        ) => matches!(
            job_record.state,
            ExecutionJobState::Queued | ExecutionJobState::Leased | ExecutionJobState::Running
        ),
        (
            ManagedAppOperation::Start | ManagedAppOperation::Restart,
            winwincode_client_port::managed_app::ManagedAppMode::FrozenCandidate,
        ) => job_record.state == ExecutionJobState::Completed,
    };
    if allowed {
        Ok(())
    } else {
        Err(ClientSessionsError::invalid_request())
    }
}

fn validate_managed_app_command_against_authority(
    command: &ManagedAppCommand,
    authoritative: &ManagedAppRunConfig,
) -> Result<(), ClientSessionsError> {
    if command.run_id != authoritative.run_id {
        return Err(ClientSessionsError::invalid_request());
    }
    match command.operation {
        ManagedAppOperation::Start | ManagedAppOperation::Restart => {
            if command.config.as_ref() != Some(authoritative) {
                return Err(ClientSessionsError::invalid_request());
            }
        }
        ManagedAppOperation::Stop | ManagedAppOperation::Query => {
            if command.config.is_some() {
                return Err(ClientSessionsError::invalid_request());
            }
        }
    }
    Ok(())
}

/// Enqueues one managed-app command after checking the durable occupancy
/// holder, lifecycle state, lease id, fencing token, and Device mirror
/// revision. The append is the Server scheduling point; the Device repeats
/// the checks before touching a local process.
///
/// # Errors
///
/// Returns a stable client-session error when any durable authority or
/// occupancy check fails.
pub fn enqueue_managed_app_command(
    data_directory: &Path,
    storage: &mut SqliteStorage,
    node: &ClientNodeRecord,
    user_id: &str,
    command: ManagedAppCommand,
    now: &Instant,
) -> Result<(), ClientSessionsError> {
    let authoritative =
        load_authoritative_managed_app_config(storage, &command.run_id, command.operation)?;
    validate_managed_app_command_against_authority(&command, &authoritative)?;
    let lease = ClientOccupancyService::new(storage)
        .active_lease_for_node(&node.client_node_id)
        .map_err(|_| ClientSessionsError::unavailable())?
        .ok_or_else(|| {
            ClientSessionsError::new(
                ClientSessionsErrorKind::OccupancyRequired,
                "the client is not occupied; claim occupancy before managing an application",
            )
        })?;
    if lease.holder_user_id != user_id {
        return Err(ClientSessionsError::new(
            ClientSessionsErrorKind::NotHolder,
            "only the occupancy holder may manage an application",
        ));
    }
    if !matches!(
        lease.state,
        OccupancyLeaseState::Occupied | OccupancyLeaseState::Draining
    ) {
        return Err(ClientSessionsError::new(
            ClientSessionsErrorKind::OccupancyRequired,
            "the occupancy is not confirmed by the device",
        ));
    }
    if command.occupancy_lease_id != lease.occupancy_lease_id
        || command.occupancy_fencing_token != lease.fencing_token
    {
        return Err(ClientSessionsError::new(
            ClientSessionsErrorKind::OccupancyRequired,
            "managed application command has a stale occupancy stamp",
        ));
    }
    let mirror_revision = client_mirror_revision_view(data_directory, &node.client_node_id)
        .map_err(|_| ClientSessionsError::unavailable())?;
    let occupancy = occupancy_stamp(
        mirror_revision,
        &lease.occupancy_lease_id,
        lease.fencing_token,
        &command.idempotency_key,
    );
    let message = managed_app_command_message(&occupancy, command)?;
    enqueue_frame(storage, node, message, now)
}

/// Builds one `client.worker.stop` downlink message (contract
/// `client-control-port-v1.md`): the stamped occupancy context, the worker
/// session and worker to stop, and the reason. The supervisor and release
/// flows enqueue this through the durable outbox so a device that is offline
/// still receives the stop after it reconnects.
#[must_use]
pub fn worker_stop_message(
    occupancy: OccupancyCommandContext,
    worker_session_id: &str,
    worker_id: &str,
    reason: ClientWorkerStopReason,
) -> ServerToClientMessage {
    ServerToClientMessage::WorkerStop(ServerWorkerStopPayload {
        occupancy,
        worker_session_id: worker_session_id.to_owned(),
        worker_id: worker_id.to_owned(),
        reason,
    })
}

/// Enqueues one Server → Client frame into the durable outbox at the next
/// free stream position.
fn enqueue_frame(
    storage: &mut SqliteStorage,
    node: &ClientNodeRecord,
    message: ServerToClientMessage,
    now: &Instant,
) -> Result<(), ClientSessionsError> {
    let instance = node
        .current_instance_id
        .clone()
        .ok_or_else(ClientSessionsError::unavailable)?;
    let cursors = {
        let mut registry = ClientRegistryService::new(storage);
        registry
            .exchange_cursors(&node.client_node_id)
            .map_err(|_| ClientSessionsError::unavailable())?
            .ok_or_else(ClientSessionsError::unavailable)?
    };
    let mut downlink = storage
        .client_downlink_outbox()
        .map_err(|_| ClientSessionsError::unavailable())?;
    let outbox_high_water = downlink
        .high_water(&node.client_node_id)
        .map_err(|_| ClientSessionsError::unavailable())?;
    let sequence = cursors
        .server_to_client_ack_sequence
        .max(outbox_high_water)
        .checked_add(1)
        .ok_or_else(ClientSessionsError::unavailable)?;
    let envelope = ServerToClientEnvelope {
        schema_version: CLIENT_CONTROL_PORT_SCHEMA_VERSION.to_owned(),
        message_id: generate_prefixed_id("msg_")?,
        client_node_id: node.client_node_id.clone(),
        client_instance_id: instance,
        sequence,
        occurred_at: now.0.clone(),
        message,
    };
    let codec = FrameCodec::new(DEFAULT_MAX_FRAME_BYTES);
    let stored = codec
        .encode_envelope(&envelope)
        .map_err(|_| ClientSessionsError::unavailable())?;
    let frame = std::str::from_utf8(&stored.frame)
        .map_err(|_| ClientSessionsError::unavailable())?
        .to_owned();
    downlink
        .append(
            &ClientDownlinkAppend::try_new(
                node.client_node_id.clone(),
                envelope.message_id.clone(),
                sequence,
                frame,
            )
            .map_err(|_| ClientSessionsError::unavailable())?,
            now,
        )
        .map_err(|_| ClientSessionsError::unavailable())?;
    Ok(())
}

/// Reads one required public Client ID: 9-12 ASCII digits.
fn required_client_id(value: Option<&Value>) -> Result<String, ClientSessionsError> {
    let text = value
        .and_then(Value::as_str)
        .ok_or_else(ClientSessionsError::invalid_request)?;
    if (9..=12).contains(&text.len()) && text.bytes().all(|byte| byte.is_ascii_digit()) {
        Ok(text.to_owned())
    } else {
        Err(ClientSessionsError::invalid_request())
    }
}

/// The canonical application instant the boundary shares across one flow.
fn now_instant() -> Instant {
    use crate::application::StandaloneApplicationClock as _;
    crate::application::SystemStandaloneApplicationClock.now_instant()
}

/// Signed millisecond amount of one duration, clamped to the `i64` range.
fn duration_millis(duration: std::time::Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

/// Crockford Base32 alphabet shared with the canonical identity encodings.
const IDENTITY_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Generates one canonical `prefix` + 26 character Crockford identifier.
fn generate_prefixed_id(prefix: &str) -> Result<String, ClientSessionsError> {
    let mut random = [0_u8; 13];
    getrandom::fill(&mut random).map_err(|_| ClientSessionsError::unavailable())?;
    let mut identity = String::with_capacity(prefix.len() + 26);
    identity.push_str(prefix);
    for byte in random {
        identity.push(IDENTITY_ALPHABET[usize::from(byte >> 4)] as char);
        identity.push(IDENTITY_ALPHABET[usize::from(byte & 0x0f)] as char);
    }
    Ok(identity)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_rejects_zero_bounds() {
        let mut config = ClientSessionsConfig::default();
        assert!(ClientSessionsApplication::open("unused", &config).is_ok());
        config.launch_wait = std::time::Duration::ZERO;
        assert!(ClientSessionsApplication::open("unused", &config).is_err());
    }

    #[test]
    fn generated_ids_carry_the_launch_prefixes() {
        for prefix in ["wlg_", "wsn_", "wrk_", "wki_", "ps_", "run_", "msg_"] {
            let id = generate_prefixed_id(prefix).expect("entropy");
            assert_eq!(id.len(), prefix.len() + 26);
            assert!(id.starts_with(prefix));
        }
    }

    #[test]
    fn public_client_id_shape_is_nine_to_twelve_digits() {
        let value = |text: &str| Some(Value::String(text.to_owned()));
        assert_eq!(
            required_client_id(value("927351842").as_ref()).expect("valid"),
            "927351842"
        );
        assert!(required_client_id(value("12345678").as_ref()).is_err());
        assert!(required_client_id(value("1234567890123").as_ref()).is_err());
        assert!(required_client_id(None).is_err());
    }

    #[test]
    fn stop_message_carries_the_stamped_occupancy_context() {
        let message = worker_stop_message(
            occupancy_stamp(7, "ocl_A", 9, "idem_stop_wlg"),
            "wsn_A",
            "wrk_A",
            ClientWorkerStopReason::GrantRevoked,
        );
        let ServerToClientMessage::WorkerStop(payload) = &message else {
            panic!("worker stop message expected");
        };
        assert_eq!(payload.worker_session_id, "wsn_A");
        assert_eq!(payload.worker_id, "wrk_A");
        assert_eq!(payload.reason, ClientWorkerStopReason::GrantRevoked);
        assert_eq!(payload.occupancy.occupancy_lease_id, "ocl_A");
        assert_eq!(payload.occupancy.occupancy_fencing_token, 9);
        assert_eq!(payload.occupancy.command.expected_revision, 7);
        assert_eq!(payload.occupancy.command.idempotency_key, "idem_stop_wlg");
    }

    #[test]
    fn managed_app_command_requires_the_current_occupancy_stamp() {
        let command = ManagedAppCommand {
            schema_version: MANAGED_APP_RUN_CONFIG_SCHEMA_VERSION.to_owned(),
            operation: ManagedAppOperation::Query,
            idempotency_key: "idem_query_app".to_owned(),
            occupancy_lease_id: "ocl_A".to_owned(),
            occupancy_fencing_token: 9,
            config: None,
            run_id: "run_A".to_owned(),
        };
        let occupancy = occupancy_stamp(7, "ocl_A", 9, "idem_query_app");
        let message = managed_app_command_message(&occupancy, command.clone())
            .expect("current lease is accepted");
        assert!(matches!(
            message,
            ServerToClientMessage::ManagedAppCommand(_)
        ));
        let foreign_occupancy = occupancy_stamp(7, "ocl_B", 9, "idem_query_app");
        let error = managed_app_command_message(&foreign_occupancy, command)
            .expect_err("foreign lease is rejected");
        assert_eq!(error.kind(), ClientSessionsErrorKind::OccupancyRequired);
    }

    #[test]
    fn managed_app_response_projects_device_health_for_the_browser() {
        let command = ManagedAppCommand {
            schema_version: MANAGED_APP_RUN_CONFIG_SCHEMA_VERSION.to_owned(),
            operation: ManagedAppOperation::Start,
            idempotency_key: "idem_start_app".to_owned(),
            occupancy_lease_id: "ocl_A".to_owned(),
            occupancy_fencing_token: 9,
            config: Some(test_managed_app_config("run_A")),
            run_id: "run_A".to_owned(),
        };
        let status = ManagedAppStatus {
            schema_version: MANAGED_APP_RUN_CONFIG_SCHEMA_VERSION.to_owned(),
            run_id: "run_A".to_owned(),
            state: ManagedAppState::Healthy,
            pid: Some(42),
            process_start_identity: Some("pid-start".to_owned()),
            exit_code: None,
            source_id: "src_A".to_owned(),
        };
        let response = managed_app_response(&command, Some(&status));
        assert_eq!(
            response["schemaVersion"],
            MANAGED_APP_RUN_CONFIG_SCHEMA_VERSION
        );
        assert_eq!(response["runId"], "run_A");
        assert_eq!(response["leaseId"], "ocl_A");
        assert_eq!(response["phase"], "ready");
    }

    #[test]
    fn managed_app_commands_use_the_persisted_run_config() {
        let directory = std::env::temp_dir().join(format!(
            "winwincode-server-managed-app-{}",
            std::process::id()
        ));
        let mut storage = SqliteStorage::open(&directory).expect("storage");
        let config = test_managed_app_config("run_A");
        storage
            .save_managed_app_run_config(&winwincode_storage::ManagedAppRunConfigRecord {
                run_id: config.run_id.clone(),
                work_run_id: config.run_id.clone(),
                repository_binding_id: config.repository_binding_id.clone(),
                attempt: i64::from(config.attempt),
                config_json: serde_json::to_vec(&config).expect("config json"),
            })
            .expect("persist config");
        let authority = config.clone();
        let mut command = ManagedAppCommand {
            schema_version: MANAGED_APP_RUN_CONFIG_SCHEMA_VERSION.to_owned(),
            operation: ManagedAppOperation::Start,
            idempotency_key: "idem_start_app".to_owned(),
            occupancy_lease_id: "ocl_A".to_owned(),
            occupancy_fencing_token: 9,
            config: Some(authority.clone()),
            run_id: "run_A".to_owned(),
        };
        validate_managed_app_command_against_authority(&command, &authority)
            .expect("stored config accepted");
        command.config.as_mut().expect("config").listen_port += 1;
        assert!(validate_managed_app_command_against_authority(&command, &authority).is_err());
        drop(storage);
        std::fs::remove_dir_all(directory).expect("cleanup");
    }

    fn test_managed_app_config(run_id: &str) -> ManagedAppRunConfig {
        ManagedAppRunConfig {
            schema_version: MANAGED_APP_RUN_CONFIG_SCHEMA_VERSION.to_owned(),
            run_id: run_id.to_owned(),
            repository_binding_id: "rb_A".to_owned(),
            template_revision: 1,
            attempt: 1,
            mode: winwincode_client_port::managed_app::ManagedAppMode::Live,
            candidate_commit: None,
            cwd: "web".to_owned(),
            argv: vec!["node".to_owned(), "server.js".to_owned()],
            env: std::collections::BTreeMap::new(),
            health_check: winwincode_client_port::managed_app::ManagedAppHealthCheck {
                path: "/".to_owned(),
                timeout_ms: 5_000,
            },
            listen_port: 4_311,
            source_id: "src_A".to_owned(),
        }
    }
}
