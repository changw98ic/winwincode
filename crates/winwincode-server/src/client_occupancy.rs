// SPDX-License-Identifier: Apache-2.0

//! User-facing Client occupancy flow over the durable occupancy ledger (plan
//! §12, contract `client-control-state-machines.md` 4, contract
//! `client-control-port-v1.md` occupancy frames).
//!
//! `POST /api/v1/clients/occupancy` applies the five-condition claim gate
//! through `ClientOccupancyService::atomic_claim`, enqueues the
//! `client.occupancy.offer` downlink frame into the durable outbox, and waits
//! a bounded, configurable interval for the Device Client's
//! `client.occupancy.ack` to be settled by the client exchange
//! (`record_acknowledgement`). Only the ACK promotes the lease to `occupied`;
//! a device rejection (`client.occupancy.rejected`), an applicant withdrawal
//! (release while still `reserving`), or the elapsed ACK window each
//! terminate the lease as `released` so `reserving` never becomes stable.
//!
//! `DELETE /api/v1/clients/occupancy` maps the three release modes of plan
//! §12.4 onto `request_release`: no active worker session releases
//! immediately, active sessions move the lease to `draining` while the
//! matching `client.occupancy.release` frame tells the device whether to let
//! tasks finish (`drain`) or cancel them (`cancel_and_release`, which
//! requires an explicit confirmation flag). A `draining` lease releases
//! automatically once the device reports zero running sessions through its
//! heartbeat.
//!
//! `POST /api/v1/clients/occupancy/force-release` is the Owner-only safe
//! cleanup of plan §12.5: it releases a `recovery_pending` lease whose
//! recovery window has passed and mints a strictly higher fencing token that
//! goes downlink as `client.occupancy.force_fence` so the device rejects
//! every command stamped with any older token.
//!
//! `GET /api/v1/clients/{clientId}/occupancy` projects the occupancy state.
//! The holder receives the full view (lease identity, fencing token,
//! capacity, recovery deadline); every other user receives only
//! `{occupancy: "occupied-by-other"}` — the holder identity is never
//! disclosed to a non-holder (plan §16.4).
//!
//! The offline sweep integration keeps the lease state machine aligned with
//! presence: whenever the heartbeat sweep projects unreachable devices to
//! `offline`, every `occupied` or `draining` lease of those devices is
//! projected to `recovery_pending` with a configurable recovery deadline.
//! Past the deadline nothing happens automatically — the occupancy is never
//! handed to a new user; the Owner force-release is the explicit resolution
//! path.
//!
//! Every occupancy downlink command (offer, release, force fence) stamps its
//! `expectedRevision` with the Server's current view of the Device Client's
//! durable occupancy mirror revision (contract `client-control-port-v1.md`:
//! Server → Client commands are computed against "Server 计算命令所依据的
//! Client 已确认镜像 revision"). The device refuses any stamp whose revision
//! is not exactly its local mirror revision, so the view is tracked durably
//! per client node in a small sidecar database: the client exchange settles
//! the device's reported facts (`client.occupancy.ack` mirror revision,
//! `client.occupancy.rejected` current revision, and the
//! `client.command_ack` effective revision of release and force-fence
//! commands) into it, and the flows here read it back when they construct a
//! stamp. A device rejection therefore re-syncs the view, and the next claim
//! recomputes against the revision the device actually reported.

use std::fmt;
use std::path::Path;
use std::path::PathBuf;

use crate::client_exchange::is_canonical_client_node_id;
use rusqlite::OptionalExtension;
use rusqlite::params;
use serde_json::Value;
use serde_json::json;
use winwincode_client_port::domain::ClientOccupancyForceFenceReason;
use winwincode_client_port::domain::ClientOccupancyReleaseMode;
use winwincode_client_port::exchange::DEFAULT_MAX_FRAME_BYTES;
use winwincode_client_port::exchange::FrameCodec;
use winwincode_client_port::messages::CLIENT_CONTROL_PORT_SCHEMA_VERSION;
use winwincode_client_port::messages::CommandContext;
use winwincode_client_port::messages::OccupancyCommandContext;
use winwincode_client_port::messages::ServerOccupancyForceFencePayload;
use winwincode_client_port::messages::ServerOccupancyOfferPayload;
use winwincode_client_port::messages::ServerOccupancyReleasePayload;
use winwincode_client_port::messages::ServerToClientEnvelope;
use winwincode_client_port::messages::ServerToClientMessage;
use winwincode_control_plane::ClientOccupancyService;
use winwincode_control_plane::ClientOccupancyServiceErrorKind;
use winwincode_control_plane::ClientRegistryService;
use winwincode_control_plane::ConnectCodeService;
use winwincode_control_plane::OccupancyLeaseState;
use winwincode_domain::Instant;
use winwincode_storage::AttemptDimension;
use winwincode_storage::ClientDownlinkAppend;
use winwincode_storage::ClientNodeRecord;
use winwincode_storage::ClientPresenceState;
use winwincode_storage::OccupancyClaim;
use winwincode_storage::OccupancyLeaseRecord;
use winwincode_storage::OccupancyReleaseReason;
use winwincode_storage::SqliteStorage;
use winwincode_storage::connect_attempt_window_anchor;

/// Schema version of the public browser-facing occupancy surface.
const SUPPORTED_SCHEMA_VERSION: &str = "winwincode/v1";

/// Bounded-wait, recovery, sweep, and throttling policy of the occupancy
/// flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientOccupancyConfig {
    /// How long one claim waits for the Device Client occupancy
    /// acknowledgement before the offer is rolled back (plan 12.2).
    pub offer_wait: std::time::Duration,
    /// How often the durable lease state is polled while waiting.
    pub poll_interval: std::time::Duration,
    /// Recovery window granted to a dropped device before the lease becomes
    /// eligible for the explicit safe cleanup (plan 12.5). The deadline never
    /// triggers an automatic release.
    pub recovery_window: std::time::Duration,
    /// How long a device must have been silent (last heartbeat) before the
    /// offline sweep projects it to `offline`.
    pub heartbeat_stale_after: std::time::Duration,
    /// How often the server-level sweep task runs.
    pub sweep_interval: std::time::Duration,
    /// Length of the fixed failed-claim window in seconds.
    pub rate_window_seconds: u64,
    /// Failed claim attempts per window and dimension that block further
    /// claims.
    pub rate_max_attempts: u64,
}

impl Default for ClientOccupancyConfig {
    fn default() -> Self {
        Self {
            offer_wait: std::time::Duration::from_secs(30),
            poll_interval: std::time::Duration::from_millis(200),
            recovery_window: std::time::Duration::from_mins(10),
            heartbeat_stale_after: std::time::Duration::from_secs(45),
            sweep_interval: std::time::Duration::from_secs(15),
            rate_window_seconds: 300,
            rate_max_attempts: 5,
        }
    }
}

/// Stable failure categories of the occupancy flow boundary. Each occupancy
/// category maps to exactly one wire error code of the central occupancy
/// error-code table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClientOccupancyErrorKind {
    /// The request body violated the occupancy contract.
    InvalidRequest,
    /// `cancel_and_release` was requested without the explicit confirmation.
    ConfirmationRequired,
    /// The public Client ID does not name an occupancy-capable Client.
    ClientNotFound,
    /// The Client is not reachable (offline, degraded, or did not answer the
    /// offer within the bounded wait).
    ClientOffline,
    /// The Client is locked by a local operator.
    ClientLocked,
    /// The Client no longer accepts new occupancy.
    ClientConnectionsForbidden,
    /// The user holds no active `use` grant on the Client.
    AccessDenied,
    /// An active lease of another user occupies the Client.
    OccupiedByOther,
    /// The Client has no free worker-session slot.
    CapacityExhausted,
    /// The Device Client rejected the occupancy offer.
    OccupancyRejected,
    /// The Device Client did not confirm the offer within the bounded wait;
    /// the lease was rolled back to `released`.
    OccupancyAckTimeout,
    /// The lease is `recovery_pending` and its recovery window is still open.
    OccupancyRecoveryPending,
    /// The acting user may not perform the requested occupancy change.
    PermissionDenied,
    /// No active lease matches the request.
    ResourceNotFound,
    /// The active lease is in a state that refuses the requested change.
    WrongState,
    /// One of the throttle dimensions is blocking further claims.
    RateLimited,
    /// Durable state or storage failed; nothing was decided.
    Unavailable,
}

/// Secret-free occupancy flow failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientOccupancyError {
    kind: ClientOccupancyErrorKind,
    message: String,
}

impl ClientOccupancyError {
    #[must_use]
    pub const fn kind(&self) -> ClientOccupancyErrorKind {
        self.kind
    }

    fn new(kind: ClientOccupancyErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    fn invalid_request() -> Self {
        Self::new(
            ClientOccupancyErrorKind::InvalidRequest,
            "occupancy request is invalid",
        )
    }

    fn unavailable() -> Self {
        Self::new(
            ClientOccupancyErrorKind::Unavailable,
            "client occupancy service is unavailable",
        )
    }
}

impl fmt::Display for ClientOccupancyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ClientOccupancyError {}

/// What one offline sweep observed, for diagnostics and tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfflineSweepOutcome {
    /// Client nodes the presence sweep projected to `offline`.
    pub swept_nodes: Vec<String>,
    /// Occupancy leases projected to `recovery_pending` by this sweep.
    pub leases_pending_recovery: Vec<String>,
}

/// The signed-in user's occupancy surface over the Server's one product-state
/// database directory. Like the connect flow, every operation opens and
/// closes its own storage connection so concurrent flows never share state in
/// memory and the bounded wait holds no database lock.
#[derive(Debug, Clone)]
pub struct ClientOccupancyApplication {
    data_directory: PathBuf,
    config: ClientOccupancyConfig,
}

/// What one validated claim prepared before the bounded wait.
enum PreparedClaim {
    /// The user already occupies the Client: an idempotent retry that returns
    /// the current holder view without a second lease.
    Completed(Value),
    /// A fresh `reserving` lease was created (or an own `reserving` lease
    /// reused) and the flow must wait for its acknowledgement.
    AwaitAck { occupancy_lease_id: String },
}

/// The outcome of one poll of a pending occupancy offer.
enum PollOutcome {
    /// The offer is still unanswered; keep waiting.
    Pending,
    /// The lease reached `occupied`; the flow returns the holder view.
    Completed,
    /// The flow failed with the mapped domain error.
    Failed(ClientOccupancyError),
}

impl ClientOccupancyApplication {
    /// Composes the occupancy application over one product-state directory.
    ///
    /// # Errors
    ///
    /// Fails when the configuration violates its bounds.
    pub fn open(
        data_directory: impl Into<PathBuf>,
        config: &ClientOccupancyConfig,
    ) -> Result<Self, ClientOccupancyError> {
        if config.offer_wait.is_zero()
            || config.poll_interval.is_zero()
            || config.recovery_window.is_zero()
            || config.heartbeat_stale_after.is_zero()
            || config.sweep_interval.is_zero()
            || config.rate_window_seconds == 0
            || config.rate_max_attempts == 0
        {
            return Err(ClientOccupancyError::new(
                ClientOccupancyErrorKind::InvalidRequest,
                "client occupancy configuration bounds must be positive",
            ));
        }
        Ok(Self {
            data_directory: data_directory.into(),
            config: config.clone(),
        })
    }

    /// Runs the full claim flow (plan 12.2) and resolves to the holder view
    /// body of the `201` response (fresh occupation and idempotent replay
    /// alike, mirroring the connect flow's retry semantics).
    ///
    /// # Errors
    ///
    /// Returns the stable occupancy failure categories; `OccupancyAckTimeout`
    /// also rolls the unanswered offer back to `released`, and
    /// `OccupancyRejected` covers a Device Client that answered with
    /// `client.occupancy.rejected`.
    pub async fn claim(
        &self,
        user_id: &str,
        request: &Value,
    ) -> Result<Value, ClientOccupancyError> {
        match self.prepare_claim(user_id, request)? {
            PreparedClaim::Completed(body) => Ok(body),
            PreparedClaim::AwaitAck { occupancy_lease_id } => {
                let deadline = tokio::time::Instant::now() + self.config.offer_wait;
                loop {
                    match self.poll_claim(user_id, &occupancy_lease_id)? {
                        PollOutcome::Pending => {}
                        PollOutcome::Completed => {
                            return self.holder_view(user_id, &occupancy_lease_id);
                        }
                        PollOutcome::Failed(error) => return Err(error),
                    }
                    if tokio::time::Instant::now() >= deadline {
                        self.roll_back_unanswered_offer(user_id, &occupancy_lease_id)?;
                        return Err(ClientOccupancyError::new(
                            ClientOccupancyErrorKind::OccupancyAckTimeout,
                            "the device did not confirm the occupancy offer in time",
                        ));
                    }
                    tokio::time::sleep(self.config.poll_interval).await;
                }
            }
        }
    }

    /// Applies the holder's release request (plan 12.4). `release` releases
    /// immediately when no worker session is active and moves the lease to
    /// `draining` otherwise; `drain` and `cancel_and_release` carry the mode
    /// downlink so the device finishes or cancels the active tasks;
    /// `cancel_and_release` requires the explicit confirmation flag. A
    /// release while the lease is still `reserving` withdraws the claim.
    ///
    /// # Errors
    ///
    /// Returns the stable occupancy failure categories.
    pub fn release(&self, user_id: &str, request: &Value) -> Result<Value, ClientOccupancyError> {
        let Some(fields) = request.as_object() else {
            return Err(ClientOccupancyError::invalid_request());
        };
        if fields.len() != 3 && fields.len() != 4 {
            return Err(ClientOccupancyError::invalid_request());
        }
        let public_client_id = required_client_id(fields.get("clientId"))?;
        let mode = match fields.get("mode").and_then(Value::as_str) {
            Some("release") => ClientOccupancyReleaseMode::Immediate,
            Some("drain") => ClientOccupancyReleaseMode::DrainThenRelease,
            Some("cancel_and_release") => ClientOccupancyReleaseMode::CancelTasksAndRelease,
            _ => return Err(ClientOccupancyError::invalid_request()),
        };
        let confirmed = fields
            .get("confirm")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if mode == ClientOccupancyReleaseMode::CancelTasksAndRelease && !confirmed {
            return Err(ClientOccupancyError::new(
                ClientOccupancyErrorKind::ConfirmationRequired,
                "cancel_and_release requires the explicit confirm flag",
            ));
        }

        let mut storage = self.open_storage()?;
        let node = ClientOccupancyApplication::lookup_node(&mut storage, &public_client_id)?;
        let now = now_instant();
        let (released_lease, state_text) = {
            let mut occupancy = ClientOccupancyService::new(&mut storage);
            let Some(lease) = occupancy
                .active_lease_for_node(&node.client_node_id)
                .map_err(|_| ClientOccupancyError::unavailable())?
            else {
                return Err(ClientOccupancyError::new(
                    ClientOccupancyErrorKind::ResourceNotFound,
                    "no active occupancy matches the requested client",
                ));
            };
            if lease.holder_user_id != user_id {
                return Err(ClientOccupancyError::new(
                    ClientOccupancyErrorKind::PermissionDenied,
                    "only the occupancy holder may release",
                ));
            }
            if lease.state == OccupancyLeaseState::Reserving {
                // The offer never completed: this is the applicant withdrawal
                // of contract 4, not a device release. No downlink frame is
                // required; a late device ACK of the withdrawn offer fails its
                // fencing judgement and changes nothing.
                occupancy
                    .reject_offer(
                        &lease.occupancy_lease_id,
                        lease.fencing_token,
                        OccupancyReleaseReason::ClaimWithdrawn,
                        &now,
                    )
                    .map_err(|_| ClientOccupancyError::unavailable())?;
                (lease, "released")
            } else if lease.state == OccupancyLeaseState::RecoveryPending {
                return Err(ClientOccupancyError::new(
                    ClientOccupancyErrorKind::WrongState,
                    "the occupancy is pending recovery; use force-release after the recovery deadline",
                ));
            } else if lease.state == OccupancyLeaseState::Draining {
                // Already draining: the first release request owns the mode,
                // and only the device-reported drain completion ends the
                // lease.
                (lease, OccupancyLeaseState::Draining.as_str())
            } else {
                // `occupied`: no active worker session releases immediately,
                // any active session moves the lease to `draining` (plan
                // 12.4). The device-reported running count is the skeleton
                // input until the execution FLOW epic lands the durable task
                // ledger.
                let released = occupancy
                    .request_release(
                        &lease.occupancy_lease_id,
                        lease.fencing_token,
                        u64::from(node.reported_running_worker_sessions),
                        &now,
                    )
                    .map_err(|_| ClientOccupancyError::unavailable())?;
                (lease, released.state.as_str())
            }
        };
        if released_lease.state == OccupancyLeaseState::Occupied {
            // The holder release command always goes downlink stamped with
            // the lease, its fencing token, and the mirror revision the
            // device last confirmed (contract 4, plan 12.4).
            let mirror_revision_view =
                client_mirror_revision_view(&self.data_directory, &node.client_node_id)
                    .map_err(|_| ClientOccupancyError::unavailable())?;
            enqueue_occupancy_frame(
                &mut storage,
                &node,
                ServerToClientMessage::OccupancyRelease(ServerOccupancyReleasePayload {
                    occupancy: occupancy_stamp(
                        mirror_revision_view,
                        &released_lease.occupancy_lease_id,
                        released_lease.fencing_token,
                        &format!("idem_release_{}", released_lease.occupancy_lease_id),
                    ),
                    mode,
                }),
                &now,
            )?;
        }
        Ok(release_body(
            &node,
            state_text,
            &released_lease.occupancy_lease_id,
            mode_text(mode),
        ))
    }

    /// Applies the Owner-only safe cleanup of a `recovery_pending` lease
    /// whose recovery window has passed (plan 12.5): the lease releases and a
    /// strictly higher fencing token goes downlink as
    /// `client.occupancy.force_fence` so the device rejects every command
    /// stamped with any older token. The occupancy is never handed to a new
    /// user automatically.
    ///
    /// # Errors
    ///
    /// Returns `WrongState` for a lease that is not `recovery_pending`,
    /// `OccupancyRecoveryPending` while the recovery window is still open,
    /// and `ResourceNotFound` when no active lease matches.
    pub fn force_release(&self, request: &Value) -> Result<Value, ClientOccupancyError> {
        let Some(fields) = request.as_object() else {
            return Err(ClientOccupancyError::invalid_request());
        };
        if fields.len() != 2 {
            return Err(ClientOccupancyError::invalid_request());
        }
        let public_client_id = required_client_id(fields.get("clientId"))?;
        let mut storage = self.open_storage()?;
        let node = ClientOccupancyApplication::lookup_node(&mut storage, &public_client_id)?;
        let now = now_instant();
        let (released, new_token) = {
            let mut occupancy = ClientOccupancyService::new(&mut storage);
            let Some(lease) = occupancy
                .active_lease_for_node(&node.client_node_id)
                .map_err(|_| ClientOccupancyError::unavailable())?
            else {
                return Err(ClientOccupancyError::new(
                    ClientOccupancyErrorKind::ResourceNotFound,
                    "no active occupancy matches the requested client",
                ));
            };
            if lease.state != OccupancyLeaseState::RecoveryPending {
                return Err(ClientOccupancyError::new(
                    ClientOccupancyErrorKind::WrongState,
                    "only a recovery-pending lease can be force-released",
                ));
            }
            let Some(deadline) = lease.recovery_deadline_at.as_ref() else {
                return Err(ClientOccupancyError::unavailable());
            };
            if now.0.as_str() < deadline.0.as_str() {
                return Err(ClientOccupancyError::new(
                    ClientOccupancyErrorKind::OccupancyRecoveryPending,
                    "the recovery window is still open",
                ));
            }
            let released = occupancy
                .force_release(&lease.occupancy_lease_id, &now)
                .map_err(|_| ClientOccupancyError::unavailable())?;
            // The node is offline while its lease is recovery pending, so no
            // claim can interleave here: the freshly minted token is strictly
            // higher than every token the device could still hold.
            let new_token = occupancy
                .mint_fencing_token()
                .map_err(|_| ClientOccupancyError::unavailable())?;
            (released, new_token)
        };
        // The device mirror revision the fence is computed against: the
        // device refuses any other stamp and would keep honouring the old
        // token.
        let mirror_revision_view =
            client_mirror_revision_view(&self.data_directory, &node.client_node_id)
                .map_err(|_| ClientOccupancyError::unavailable())?;
        enqueue_occupancy_frame(
            &mut storage,
            &node,
            ServerToClientMessage::OccupancyForceFence(ServerOccupancyForceFencePayload {
                occupancy: occupancy_stamp(
                    mirror_revision_view,
                    &released.occupancy_lease_id,
                    new_token,
                    &format!("idem_force_fence_{}", released.occupancy_lease_id),
                ),
                reason: ClientOccupancyForceFenceReason::RecoveryDeadlineExceeded,
                superseded_lease_id: Some(released.occupancy_lease_id.clone()),
            }),
            &now,
        )?;
        Ok(json!({
            "schemaVersion": SUPPORTED_SCHEMA_VERSION,
            "clientId": node.public_client_id,
            "released": true,
            "occupancyLeaseId": released.occupancy_lease_id,
            "forceFenceToken": new_token,
        }))
    }

    /// Projects the occupancy state of one Client for the acting user
    /// (plan §16.4): the holder receives the full view, every other user only
    /// the privacy projection that never discloses the holder identity.
    ///
    /// # Errors
    ///
    /// Returns `ClientNotFound` for an unknown or unenrolled Client and
    /// `Unavailable` for storage failure.
    pub fn status(
        &self,
        user_id: &str,
        public_client_id: &str,
    ) -> Result<Value, ClientOccupancyError> {
        if !is_public_client_id(public_client_id) {
            return Err(ClientOccupancyError::invalid_request());
        }
        let mut storage = self.open_storage()?;
        let node = ClientOccupancyApplication::lookup_node(&mut storage, public_client_id)?;
        let mut occupancy = ClientOccupancyService::new(&mut storage);
        let Some(lease) = occupancy
            .active_lease_for_node(&node.client_node_id)
            .map_err(|_| ClientOccupancyError::unavailable())?
        else {
            return Ok(json!({
                "schemaVersion": SUPPORTED_SCHEMA_VERSION,
                "clientId": node.public_client_id,
                "occupancy": "available",
                "presence": presence_text(&node),
            }));
        };
        if lease.holder_user_id != user_id {
            // Privacy projection: a non-holder learns that the Client is in
            // use and nothing else — never the holder identity, never lease
            // or task details (plan §16.4).
            return Ok(json!({
                "schemaVersion": SUPPORTED_SCHEMA_VERSION,
                "clientId": node.public_client_id,
                "occupancy": "occupied-by-other",
            }));
        }
        Ok(holder_view_json(&node, &lease))
    }

    /// Runs one offline sweep integration step: projects devices whose last
    /// accepted heartbeat is at or before `cutoff` to `offline`, then
    /// projects every `occupied` or `draining` lease of those devices to
    /// `recovery_pending` with a `now + recovery_window` deadline (plan
    /// 12.5). Past the deadline nothing is released automatically.
    ///
    /// # Errors
    ///
    /// Fails when the presence sweep hits a storage failure; an individual
    /// lease that cannot be marked (for example one released concurrently) is
    /// skipped, never fails the sweep.
    pub fn run_offline_sweep(
        &self,
        cutoff: &Instant,
        now: &Instant,
    ) -> Result<OfflineSweepOutcome, ClientOccupancyError> {
        let mut storage = self.open_storage()?;
        let swept = {
            let mut registry = ClientRegistryService::new(&mut storage);
            registry
                .sweep_offline(cutoff)
                .map_err(|_| ClientOccupancyError::unavailable())?
        };
        let deadline = offset_instant(now, duration_millis(self.config.recovery_window))
            .ok_or_else(ClientOccupancyError::unavailable)?;
        let mut leases_pending_recovery = Vec::new();
        for node_id in &swept {
            let mut occupancy = ClientOccupancyService::new(&mut storage);
            let Ok(Some(lease)) = occupancy.active_lease_for_node(node_id) else {
                continue;
            };
            if !matches!(
                lease.state,
                OccupancyLeaseState::Occupied | OccupancyLeaseState::Draining
            ) {
                continue;
            }
            if occupancy
                .mark_recovery_pending(&lease.occupancy_lease_id, &deadline)
                .is_ok()
            {
                leases_pending_recovery.push(lease.occupancy_lease_id);
            }
        }
        Ok(OfflineSweepOutcome {
            swept_nodes: swept,
            leases_pending_recovery,
        })
    }

    /// Runs one production sweep step for the server-level background task:
    /// the cutoff and now derive from the application clock and the
    /// configured heartbeat staleness window.
    ///
    /// # Errors
    ///
    /// Propagates the [`Self::run_offline_sweep`] failures.
    pub fn run_server_sweep(&self) -> Result<OfflineSweepOutcome, ClientOccupancyError> {
        let now = now_instant();
        let cutoff = offset_instant(&now, -duration_millis(self.config.heartbeat_stale_after))
            .ok_or_else(ClientOccupancyError::unavailable)?;
        self.run_offline_sweep(&cutoff, &now)
    }

    /// Reports the recovery-overdue projection of one Client: whether its
    /// active `recovery_pending` lease has passed its recovery deadline and
    /// is therefore eligible for the explicit Owner safe cleanup. Nothing is
    /// released automatically.
    ///
    /// # Errors
    ///
    /// Fails when the storage lookup fails.
    pub fn recovery_overdue(&self, public_client_id: &str) -> Result<bool, ClientOccupancyError> {
        if !is_public_client_id(public_client_id) {
            return Err(ClientOccupancyError::invalid_request());
        }
        let mut storage = self.open_storage()?;
        let node = ClientOccupancyApplication::lookup_node(&mut storage, public_client_id)?;
        let mut occupancy = ClientOccupancyService::new(&mut storage);
        let Some(lease) = occupancy
            .active_lease_for_node(&node.client_node_id)
            .map_err(|_| ClientOccupancyError::unavailable())?
        else {
            return Ok(false);
        };
        if lease.state != OccupancyLeaseState::RecoveryPending {
            return Ok(false);
        }
        let now = now_instant();
        Ok(lease
            .recovery_deadline_at
            .as_ref()
            .is_some_and(|deadline| now.0.as_str() >= deadline.0.as_str()))
    }

    /// Validates the claim request and durable preconditions, creates the
    /// `reserving` lease through the atomic five-condition gate, and
    /// enqueues the `client.occupancy.offer` downlink frame.
    #[allow(clippy::too_many_lines)]
    fn prepare_claim(
        &self,
        user_id: &str,
        request: &Value,
    ) -> Result<PreparedClaim, ClientOccupancyError> {
        let Some(fields) = request.as_object() else {
            return Err(ClientOccupancyError::invalid_request());
        };
        if fields.len() != 2 {
            return Err(ClientOccupancyError::invalid_request());
        }
        let public_client_id = required_client_id(fields.get("clientId"))?;

        let mut storage = self.open_storage()?;
        let node = ClientOccupancyApplication::lookup_node(&mut storage, &public_client_id)?;

        // Claim throttling: per user and per target Client, against the same
        // fixed window shape as the connect flow.
        {
            let mut connect = ConnectCodeService::new(&mut storage);
            let anchor =
                connect_attempt_window_anchor(&now_instant(), self.config.rate_window_seconds)
                    .map_err(|_| ClientOccupancyError::unavailable())?;
            for (dimension, subject) in [
                (AttemptDimension::User, user_id),
                (AttemptDimension::Client, node.client_node_id.as_str()),
            ] {
                let blocked = connect
                    .connect_attempts_blocked(
                        dimension,
                        subject,
                        &anchor,
                        self.config.rate_max_attempts,
                    )
                    .map_err(|_| ClientOccupancyError::unavailable())?;
                if blocked {
                    return Err(ClientOccupancyError::new(
                        ClientOccupancyErrorKind::RateLimited,
                        "occupancy claims are rate limited",
                    ));
                }
            }
        }

        // An own active lease is an idempotent replay or a waitable offer;
        // any other active lease is the occupancy conflict of the gate.
        {
            let mut occupancy = ClientOccupancyService::new(&mut storage);
            if let Some(lease) = occupancy
                .active_lease_for_node(&node.client_node_id)
                .map_err(|_| ClientOccupancyError::unavailable())?
            {
                if lease.holder_user_id == user_id {
                    match lease.state {
                        OccupancyLeaseState::Reserving => {
                            return Ok(PreparedClaim::AwaitAck {
                                occupancy_lease_id: lease.occupancy_lease_id,
                            });
                        }
                        OccupancyLeaseState::Occupied => {
                            return Ok(PreparedClaim::Completed(holder_view_json(&node, &lease)));
                        }
                        OccupancyLeaseState::Draining => {
                            return Err(ClientOccupancyError::new(
                                ClientOccupancyErrorKind::WrongState,
                                "your occupancy is draining",
                            ));
                        }
                        OccupancyLeaseState::RecoveryPending => {
                            return Err(ClientOccupancyError::new(
                                ClientOccupancyErrorKind::WrongState,
                                "your occupancy is pending recovery",
                            ));
                        }
                        OccupancyLeaseState::Released | OccupancyLeaseState::Expired => {}
                    }
                } else {
                    self.record_claim_failures(user_id, &node.client_node_id)?;
                    return Err(ClientOccupancyError::new(
                        ClientOccupancyErrorKind::OccupiedByOther,
                        "the client is occupied by another user",
                    ));
                }
            }
        }

        let now = now_instant();
        let claim = OccupancyClaim::try_new(
            generate_prefixed_id("ocl_").map_err(|_| ClientOccupancyError::unavailable())?,
            node.client_node_id.clone(),
            user_id,
            generate_prefixed_id("req_").map_err(|_| ClientOccupancyError::unavailable())?,
        )
        .map_err(|_| ClientOccupancyError::unavailable())?;
        let lease = {
            let mut occupancy = ClientOccupancyService::new(&mut storage);
            match occupancy.atomic_claim(&claim, &now) {
                Ok(lease) => lease,
                Err(error) => {
                    if claim_gate_failure_is_throttled(error.kind()) {
                        self.record_claim_failures(user_id, &node.client_node_id)?;
                    }
                    return Err(claim_gate_error(error.kind()));
                }
            }
        };
        // The offer is computed against the mirror revision the device last
        // confirmed: the device refuses any other stamp.
        let mirror_revision_view =
            client_mirror_revision_view(&self.data_directory, &node.client_node_id)
                .map_err(|_| ClientOccupancyError::unavailable())?;
        enqueue_occupancy_frame(
            &mut storage,
            &node,
            ServerToClientMessage::OccupancyOffer(ServerOccupancyOfferPayload {
                occupancy: occupancy_stamp(
                    mirror_revision_view,
                    &lease.occupancy_lease_id,
                    lease.fencing_token,
                    &format!("idem_offer_{}", lease.occupancy_lease_id),
                ),
                claim_request_id: lease.claim_request_id.clone(),
                claimed_at: lease.claimed_at.clone().unwrap_or_else(|| now.clone()).0,
                holder_user_id: lease.holder_user_id.clone(),
                idle_expires_at: lease
                    .idle_expires_at
                    .as_ref()
                    .map(|instant| instant.0.clone()),
            }),
            &now,
        )?;
        Ok(PreparedClaim::AwaitAck {
            occupancy_lease_id: lease.occupancy_lease_id,
        })
    }

    /// Reads the durable lease state once and drives the claim to its next
    /// transition (plan 12.2, contract 9.3).
    fn poll_claim(
        &self,
        user_id: &str,
        occupancy_lease_id: &str,
    ) -> Result<PollOutcome, ClientOccupancyError> {
        let mut storage = self.open_storage()?;
        let mut occupancy = ClientOccupancyService::new(&mut storage);
        let lease = occupancy
            .snapshot(occupancy_lease_id)
            .map_err(|_| ClientOccupancyError::unavailable())?;
        let Some(lease) = lease else {
            return Ok(PollOutcome::Failed(ClientOccupancyError::unavailable()));
        };
        match lease.state {
            OccupancyLeaseState::Reserving => Ok(PollOutcome::Pending),
            OccupancyLeaseState::Occupied => Ok(PollOutcome::Completed),
            OccupancyLeaseState::Released => {
                self.record_claim_failures(user_id, &lease.client_node_id)?;
                Ok(PollOutcome::Failed(ClientOccupancyError::new(
                    ClientOccupancyErrorKind::OccupancyRejected,
                    "the device rejected the occupancy offer",
                )))
            }
            OccupancyLeaseState::Draining
            | OccupancyLeaseState::RecoveryPending
            | OccupancyLeaseState::Expired => Ok(PollOutcome::Failed(ClientOccupancyError::new(
                ClientOccupancyErrorKind::WrongState,
                "the occupancy lease left the reserving state unexpectedly",
            ))),
        }
    }

    /// Terminates the unanswered offer as `released` with the `ack_timeout`
    /// reason (contract 4: `reserving` is never stable) and feeds the fixed
    /// claim throttle.
    fn roll_back_unanswered_offer(
        &self,
        user_id: &str,
        occupancy_lease_id: &str,
    ) -> Result<(), ClientOccupancyError> {
        let mut storage = self.open_storage()?;
        let client_node_id = {
            let mut occupancy = ClientOccupancyService::new(&mut storage);
            let Some(lease) = occupancy
                .snapshot(occupancy_lease_id)
                .map_err(|_| ClientOccupancyError::unavailable())?
            else {
                return Err(ClientOccupancyError::unavailable());
            };
            if lease.state == OccupancyLeaseState::Reserving {
                occupancy
                    .reject_offer(
                        occupancy_lease_id,
                        lease.fencing_token,
                        OccupancyReleaseReason::AckTimeout,
                        &now_instant(),
                    )
                    .map_err(|_| ClientOccupancyError::unavailable())?;
            }
            lease.client_node_id
        };
        self.record_claim_failures(user_id, &client_node_id)
    }

    /// Builds the full holder view of one lease (plan §16.4 holder side).
    fn holder_view(
        &self,
        user_id: &str,
        occupancy_lease_id: &str,
    ) -> Result<Value, ClientOccupancyError> {
        let mut storage = self.open_storage()?;
        let lease = {
            let mut occupancy = ClientOccupancyService::new(&mut storage);
            occupancy
                .snapshot(occupancy_lease_id)
                .map_err(|_| ClientOccupancyError::unavailable())?
                .ok_or_else(ClientOccupancyError::unavailable)?
        };
        if lease.holder_user_id != user_id {
            return Err(ClientOccupancyError::new(
                ClientOccupancyErrorKind::PermissionDenied,
                "only the occupancy holder may read the full view",
            ));
        }
        let node = {
            let mut registry = ClientRegistryService::new(&mut storage);
            registry
                .snapshot(&lease.client_node_id)
                .map_err(|_| ClientOccupancyError::unavailable())?
                .ok_or_else(ClientOccupancyError::unavailable)?
        };
        Ok(holder_view_json(&node, &lease))
    }

    /// Records one failed claim attempt in both throttle dimensions (user and
    /// target Client), mirroring the connect flow's fixed-window throttle.
    fn record_claim_failures(
        &self,
        user_id: &str,
        client_node_id: &str,
    ) -> Result<(), ClientOccupancyError> {
        let anchor = connect_attempt_window_anchor(&now_instant(), self.config.rate_window_seconds)
            .map_err(|_| ClientOccupancyError::unavailable())?;
        let mut storage = self.open_storage()?;
        let mut connect = ConnectCodeService::new(&mut storage);
        for (dimension, subject) in [
            (AttemptDimension::User, user_id),
            (AttemptDimension::Client, client_node_id),
        ] {
            connect
                .record_connect_failure(dimension, subject, &anchor)
                .map_err(|_| ClientOccupancyError::unavailable())?;
        }
        Ok(())
    }

    fn lookup_node(
        storage: &mut SqliteStorage,
        public_client_id: &str,
    ) -> Result<ClientNodeRecord, ClientOccupancyError> {
        let mut registry = ClientRegistryService::new(storage);
        let record = registry
            .snapshot_by_public_client_id(public_client_id)
            .map_err(|_| ClientOccupancyError::unavailable())?;
        match record {
            None
            | Some(ClientNodeRecord {
                presence_state:
                    ClientPresenceState::PendingEnrollment | ClientPresenceState::Revoked,
                ..
            }) => Err(ClientOccupancyError::new(
                ClientOccupancyErrorKind::ClientNotFound,
                "no client matches the requested id",
            )),
            Some(node) => Ok(node),
        }
    }

    fn open_storage(&self) -> Result<SqliteStorage, ClientOccupancyError> {
        SqliteStorage::open(&self.data_directory).map_err(|_| ClientOccupancyError::unavailable())
    }
}

/// Whether a claim gate failure feeds the fixed-window claim throttle.
/// Storage-level undecided failures and unknown-client lookups do not count
/// as failed claims.
const fn claim_gate_failure_is_throttled(kind: ClientOccupancyServiceErrorKind) -> bool {
    !matches!(
        kind,
        ClientOccupancyServiceErrorKind::UnknownClientNode
            | ClientOccupancyServiceErrorKind::InvalidInput
            | ClientOccupancyServiceErrorKind::OccupancyLeaseConflict
            | ClientOccupancyServiceErrorKind::FencingTokenExhausted
            | ClientOccupancyServiceErrorKind::RevisionConflict
            | ClientOccupancyServiceErrorKind::CorruptState
            | ClientOccupancyServiceErrorKind::Storage
    )
}

/// Maps one atomic claim gate failure onto the central occupancy error-code
/// taxonomy.
fn claim_gate_error(kind: ClientOccupancyServiceErrorKind) -> ClientOccupancyError {
    match kind {
        ClientOccupancyServiceErrorKind::UnknownClientNode => ClientOccupancyError::new(
            ClientOccupancyErrorKind::ClientNotFound,
            "no client matches the requested id",
        ),
        ClientOccupancyServiceErrorKind::AccessDenied => ClientOccupancyError::new(
            ClientOccupancyErrorKind::AccessDenied,
            "an active use grant on the client is required",
        ),
        ClientOccupancyServiceErrorKind::PresenceNotOnline => ClientOccupancyError::new(
            ClientOccupancyErrorKind::ClientOffline,
            "the client is not online",
        ),
        ClientOccupancyServiceErrorKind::ClientLocked => ClientOccupancyError::new(
            ClientOccupancyErrorKind::ClientLocked,
            "the client is locked",
        ),
        ClientOccupancyServiceErrorKind::NotAcceptingConnections => ClientOccupancyError::new(
            ClientOccupancyErrorKind::ClientConnectionsForbidden,
            "the client no longer accepts new occupancy",
        ),
        ClientOccupancyServiceErrorKind::CapacityExhausted => ClientOccupancyError::new(
            ClientOccupancyErrorKind::CapacityExhausted,
            "the client has no free worker-session slot",
        ),
        ClientOccupancyServiceErrorKind::ActiveLeaseConflict => ClientOccupancyError::new(
            ClientOccupancyErrorKind::OccupiedByOther,
            "the client is occupied by another user",
        ),
        _ => ClientOccupancyError::unavailable(),
    }
}

/// Builds the holder view body of one active lease: lease identity, fencing
/// token, capacity, and the recovery deadline while it is pending.
fn holder_view_json(node: &ClientNodeRecord, lease: &OccupancyLeaseRecord) -> Value {
    json!({
        "schemaVersion": SUPPORTED_SCHEMA_VERSION,
        "clientId": node.public_client_id,
        "occupancy": lease.state.as_str(),
        "presence": presence_text(node),
        "holderUserId": lease.holder_user_id,
        "occupancyLeaseId": lease.occupancy_lease_id,
        "fencingToken": lease.fencing_token,
        "claimedAt": lease.claimed_at.as_ref().map(|instant| instant.0.clone()),
        "acknowledgedAt": lease.acknowledged_at.as_ref().map(|instant| instant.0.clone()),
        "recoveryDeadlineAt": lease.recovery_deadline_at.as_ref().map(|instant| instant.0.clone()),
        "capacityUsed": node.reported_running_worker_sessions,
        "capacityTotal": node.max_concurrent_worker_sessions,
    })
}

/// Builds the release response body.
fn release_body(
    node: &ClientNodeRecord,
    occupancy: &str,
    occupancy_lease_id: &str,
    mode: &str,
) -> Value {
    json!({
        "schemaVersion": SUPPORTED_SCHEMA_VERSION,
        "clientId": node.public_client_id,
        "occupancy": occupancy,
        "occupancyLeaseId": occupancy_lease_id,
        "mode": mode,
    })
}

/// Maps the wire release mode onto its display text.
const fn mode_text(mode: ClientOccupancyReleaseMode) -> &'static str {
    match mode {
        ClientOccupancyReleaseMode::Immediate => "release",
        ClientOccupancyReleaseMode::DrainThenRelease => "drain",
        ClientOccupancyReleaseMode::CancelTasksAndRelease => "cancel_and_release",
    }
}

/// Builds the occupancy fencing stamp every occupancy downlink command
/// carries (contract `client-control-port-v1.md`, `C + L`). The stamp's
/// `expectedRevision` is the Server's current view of the Device Client's
/// durable occupancy mirror revision — the device refuses any stamp whose
/// revision is not exactly its local mirror revision.
fn occupancy_stamp(
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

// ── Server-side view of the Device Client occupancy mirror revision ──────

/// Sidecar database that tracks, per client node, the Server's view of the
/// Device Client's durable occupancy mirror revision (the same per-concern
/// sidecar pattern as the auth-session store and the event hub). The client
/// exchange observation writes every revision fact the device reports; the
/// occupancy flows read the view when they stamp a downlink command.
const MIRROR_VIEW_DATABASE_FILE: &str = "client-occupancy-mirror.sqlite3";

const MIRROR_VIEW_SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS client_occupancy_mirror_revisions (
    client_node_id TEXT PRIMARY KEY NOT NULL,
    mirror_revision INTEGER NOT NULL
        CHECK (mirror_revision >= 0 AND mirror_revision <= 9007199254740991),
    updated_at TEXT NOT NULL
);
";

/// Failure of the mirror-revision view store.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MirrorRevisionViewError {
    /// The client node id is not canonical.
    InvalidNode,
    /// The sidecar database failed.
    Storage,
}

/// Reads the Server's current view of one device's occupancy mirror
/// revision; zero before the device reported any mirror fact.
pub(crate) fn client_mirror_revision_view(
    data_directory: &Path,
    client_node_id: &str,
) -> Result<u64, MirrorRevisionViewError> {
    if !is_canonical_client_node_id(client_node_id) {
        return Err(MirrorRevisionViewError::InvalidNode);
    }
    let connection = open_mirror_view_connection(data_directory)?;
    let stored: Option<i64> = connection
        .query_row(
            "SELECT mirror_revision FROM client_occupancy_mirror_revisions
             WHERE client_node_id = ?1",
            [client_node_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|_| MirrorRevisionViewError::Storage)?;
    stored
        .map(u64::try_from)
        .transpose()
        .map_err(|_| MirrorRevisionViewError::Storage)
        .map(Option::unwrap_or_default)
}

/// Records one mirror-revision fact the device reported (the ack's
/// `mirrorRevision`, a rejection's current revision, or a command ack's
/// effective revision). The view only advances: the device mirror never
/// rolls back, so a late lower report never regresses the stamp source.
pub(crate) fn observe_client_mirror_revision(
    data_directory: &Path,
    client_node_id: &str,
    reported_revision: u64,
    now: &Instant,
) -> Result<(), MirrorRevisionViewError> {
    if !is_canonical_client_node_id(client_node_id) {
        return Err(MirrorRevisionViewError::InvalidNode);
    }
    let reported =
        i64::try_from(reported_revision).map_err(|_| MirrorRevisionViewError::Storage)?;
    let connection = open_mirror_view_connection(data_directory)?;
    connection
        .execute(
            "INSERT INTO client_occupancy_mirror_revisions
             (client_node_id, mirror_revision, updated_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT (client_node_id) DO UPDATE SET
                 mirror_revision = MAX(mirror_revision, excluded.mirror_revision),
                 updated_at = excluded.updated_at",
            params![client_node_id, reported, now.0],
        )
        .map_err(|_| MirrorRevisionViewError::Storage)?;
    Ok(())
}

/// Opens the sidecar database and ensures its schema (per-call connection,
/// like every other occupancy flow storage access).
fn open_mirror_view_connection(
    data_directory: &Path,
) -> Result<rusqlite::Connection, MirrorRevisionViewError> {
    std::fs::create_dir_all(data_directory).map_err(|_| MirrorRevisionViewError::Storage)?;
    let connection = rusqlite::Connection::open(data_directory.join(MIRROR_VIEW_DATABASE_FILE))
        .map_err(|_| MirrorRevisionViewError::Storage)?;
    connection
        .execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")
        .map_err(|_| MirrorRevisionViewError::Storage)?;
    connection
        .execute_batch(MIRROR_VIEW_SCHEMA)
        .map_err(|_| MirrorRevisionViewError::Storage)?;
    Ok(connection)
}

/// Enqueues one `client.occupancy.*` downlink frame into the durable outbox
/// at the next free stream position.
fn enqueue_occupancy_frame(
    storage: &mut SqliteStorage,
    node: &ClientNodeRecord,
    message: ServerToClientMessage,
    now: &Instant,
) -> Result<(), ClientOccupancyError> {
    let instance = node
        .current_instance_id
        .clone()
        .ok_or_else(ClientOccupancyError::unavailable)?;
    let cursors = {
        let mut registry = ClientRegistryService::new(storage);
        registry
            .exchange_cursors(&node.client_node_id)
            .map_err(|_| ClientOccupancyError::unavailable())?
            .ok_or_else(ClientOccupancyError::unavailable)?
    };
    let mut downlink = storage
        .client_downlink_outbox()
        .map_err(|_| ClientOccupancyError::unavailable())?;
    let outbox_high_water = downlink
        .high_water(&node.client_node_id)
        .map_err(|_| ClientOccupancyError::unavailable())?;
    let sequence = cursors
        .server_to_client_ack_sequence
        .max(outbox_high_water)
        .checked_add(1)
        .ok_or_else(ClientOccupancyError::unavailable)?;
    let envelope = ServerToClientEnvelope {
        schema_version: CLIENT_CONTROL_PORT_SCHEMA_VERSION.to_owned(),
        message_id: generate_prefixed_id("msg_")
            .map_err(|_| ClientOccupancyError::unavailable())?,
        client_node_id: node.client_node_id.clone(),
        client_instance_id: instance,
        sequence,
        occurred_at: now.0.clone(),
        message,
    };
    let codec = FrameCodec::new(DEFAULT_MAX_FRAME_BYTES);
    let stored = codec
        .encode_envelope(&envelope)
        .map_err(|_| ClientOccupancyError::unavailable())?;
    let frame = std::str::from_utf8(&stored.frame)
        .map_err(|_| ClientOccupancyError::unavailable())?
        .to_owned();
    downlink
        .append(
            &ClientDownlinkAppend::try_new(
                node.client_node_id.clone(),
                envelope.message_id.clone(),
                sequence,
                frame,
            )
            .map_err(|_| ClientOccupancyError::unavailable())?,
            now,
        )
        .map_err(|_| ClientOccupancyError::unavailable())?;
    Ok(())
}

/// Reads one required public Client ID: 9-12 ASCII digits.
fn required_client_id(value: Option<&Value>) -> Result<String, ClientOccupancyError> {
    let text = value
        .and_then(Value::as_str)
        .ok_or_else(ClientOccupancyError::invalid_request)?;
    if !is_public_client_id(text) {
        return Err(ClientOccupancyError::invalid_request());
    }
    Ok(text.to_owned())
}

/// Whether `value` carries the 9-12 digit public Client ID shape.
fn is_public_client_id(value: &str) -> bool {
    (9..=12).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_digit())
}

/// Maps the registry presence onto the three-value display presence (§12.1).
const fn presence_text(record: &ClientNodeRecord) -> &'static str {
    match record.presence_state {
        ClientPresenceState::Online | ClientPresenceState::Degraded => "online",
        ClientPresenceState::Locked => "locked",
        ClientPresenceState::Offline
        | ClientPresenceState::PendingEnrollment
        | ClientPresenceState::Revoked => "offline",
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

/// Offsets one canonical millisecond application instant by a signed
/// millisecond amount, preserving the fixed `YYYY-MM-DDTHH:MM:SS.mmmZ` shape
/// the durable lexicographic comparisons rely on.
fn offset_instant(instant: &Instant, offset_millis: i64) -> Option<Instant> {
    let bytes = instant.0.as_bytes();
    if bytes.len() != 24
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[19] != b'.'
        || bytes[23] != b'Z'
    {
        return None;
    }
    let digits = |range: std::ops::Range<usize>| -> Option<i64> {
        bytes[range].iter().try_fold(0_i64, |value, byte| {
            let digit = i64::from(byte.checked_sub(b'0')?);
            (digit <= 9).then_some(value * 10 + digit)
        })
    };
    let year = digits(0..4)?;
    let month = digits(5..7)?;
    let day = digits(8..10)?;
    let hour = digits(11..13)?;
    let minute = digits(14..16)?;
    let second = digits(17..19)?;
    let millis = digits(20..23)?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let epoch_millis = days_from_civil(year, month, day) * 86_400_000
        + hour * 3_600_000
        + minute * 60_000
        + second * 1_000
        + millis;
    let shifted = epoch_millis.checked_add(offset_millis)?;
    if shifted < 0 {
        return None;
    }
    let days = shifted.div_euclid(86_400_000);
    let millis_of_day = shifted.rem_euclid(86_400_000);
    let (year, month, day) = civil_from_days(days);
    Some(Instant(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z",
        hour = millis_of_day / 3_600_000,
        minute = (millis_of_day % 3_600_000) / 60_000,
        second = (millis_of_day % 60_000) / 1_000,
        millis = millis_of_day % 1_000,
    )))
}

/// Days since the Unix epoch for a civil date (Howard Hinnant's algorithm).
const fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_of_period = (month + 9) % 12;
    let day_of_year = (153 * month_of_period + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Civil date for days since the Unix epoch (Howard Hinnant's algorithm).
const fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_of_period = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_of_period + 2) / 5 + 1;
    let month = if month_of_period < 10 {
        month_of_period + 3
    } else {
        month_of_period - 9
    };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// Crockford Base32 alphabet shared with the canonical identity encodings.
const IDENTITY_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Generates one canonical `prefix` + 26 character Crockford identifier.
fn generate_prefixed_id(prefix: &str) -> Result<String, ClientOccupancyError> {
    let mut random = [0_u8; 13];
    getrandom::fill(&mut random).map_err(|_| ClientOccupancyError::unavailable())?;
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
    use std::sync::OnceLock;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    use super::*;

    static NEXT_VIEW_DIRECTORY: AtomicU64 = AtomicU64::new(1);

    fn view_directory(label: &str) -> PathBuf {
        static NAMESPACE: OnceLock<String> = OnceLock::new();
        let namespace = NAMESPACE.get_or_init(|| {
            const HEX: &[u8; 16] = b"0123456789abcdef";
            let mut nonce = [0_u8; 8];
            getrandom::fill(&mut nonce).expect("entropy");
            let mut encoded = String::new();
            for byte in nonce {
                encoded.push(char::from(HEX[usize::from(byte >> 4)]));
                encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
            }
            format!("{}-{encoded}", std::process::id())
        });
        let id = NEXT_VIEW_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("occupancy-view-{label}-{namespace}-{id}"))
    }

    fn canonical_node(suffix_digit: char) -> String {
        format!("cnd_{suffix_digit}{}", "A".repeat(25))
    }

    #[test]
    fn the_mirror_revision_view_starts_at_zero_and_only_advances() {
        let directory = view_directory("advance");
        let node = canonical_node('A');
        assert_eq!(
            client_mirror_revision_view(&directory, &node).expect("view read"),
            0,
            "a device that reported nothing is viewed at revision zero"
        );
        let now = Instant("2026-09-04T12:00:00.000Z".to_owned());
        observe_client_mirror_revision(&directory, &node, 1, &now).expect("observe");
        assert_eq!(
            client_mirror_revision_view(&directory, &node).expect("view read"),
            1
        );
        // A later report names the advanced revision.
        observe_client_mirror_revision(&directory, &node, 3, &now).expect("observe");
        assert_eq!(
            client_mirror_revision_view(&directory, &node).expect("view read"),
            3
        );
        // The device mirror never rolls back, so a late lower report (an
        // interleaved older frame) must not regress the stamp source.
        observe_client_mirror_revision(&directory, &node, 2, &now).expect("observe");
        assert_eq!(
            client_mirror_revision_view(&directory, &node).expect("view read"),
            3
        );
    }

    #[test]
    fn the_mirror_revision_view_tracks_nodes_independently() {
        let directory = view_directory("nodes");
        let now = Instant("2026-09-04T12:00:00.000Z".to_owned());
        let first = canonical_node('B');
        let second = canonical_node('C');
        observe_client_mirror_revision(&directory, &first, 2, &now).expect("observe");
        assert_eq!(
            client_mirror_revision_view(&directory, &first).expect("view read"),
            2
        );
        assert_eq!(
            client_mirror_revision_view(&directory, &second).expect("view read"),
            0,
            "the other device reported nothing"
        );
    }

    #[test]
    fn the_mirror_revision_view_refuses_non_canonical_nodes() {
        let directory = view_directory("invalid");
        assert_eq!(
            client_mirror_revision_view(&directory, "device-local-pending"),
            Err(MirrorRevisionViewError::InvalidNode)
        );
        let now = Instant("2026-09-04T12:00:00.000Z".to_owned());
        assert_eq!(
            observe_client_mirror_revision(&directory, "nope", 1, &now),
            Err(MirrorRevisionViewError::InvalidNode)
        );
    }

    #[test]
    fn config_rejects_zero_bounds() {
        let mut config = ClientOccupancyConfig::default();
        assert!(ClientOccupancyApplication::open("unused", &config).is_ok());
        config.offer_wait = std::time::Duration::ZERO;
        assert!(ClientOccupancyApplication::open("unused", &config).is_err());
    }

    #[test]
    fn offset_instant_keeps_the_canonical_millisecond_shape() {
        let instant = Instant("2026-09-04T12:00:00.000Z".to_owned());
        assert_eq!(
            offset_instant(&instant, 61_000).expect("offset"),
            Instant("2026-09-04T12:01:01.000Z".to_owned())
        );
        assert_eq!(
            offset_instant(&instant, -1_000).expect("negative offset"),
            Instant("2026-09-04T11:59:59.000Z".to_owned())
        );
        // A year boundary rolls over correctly.
        let eve = Instant("2026-12-31T23:59:59.999Z".to_owned());
        assert_eq!(
            offset_instant(&eve, 1).expect("rollover"),
            Instant("2027-01-01T00:00:00.000Z".to_owned())
        );
        // An offset that reaches before the epoch fails closed.
        assert_eq!(offset_instant(&instant, -2_000_000_000_000_000), None);
        assert_eq!(
            offset_instant(&Instant("not-an-instant".to_owned()), 1),
            None
        );
    }

    #[test]
    fn public_client_id_shape_is_nine_to_twelve_digits() {
        assert!(is_public_client_id("927351842"));
        assert!(is_public_client_id("123456789012"));
        assert!(!is_public_client_id("12345678"));
        assert!(!is_public_client_id("1234567890123"));
        assert!(!is_public_client_id("12345678a"));
        assert!(!is_public_client_id(""));
    }

    #[test]
    fn generated_ids_carry_the_occupancy_prefixes() {
        for prefix in ["ocl_", "req_", "msg_"] {
            let id = generate_prefixed_id(prefix).expect("entropy");
            assert_eq!(id.len(), prefix.len() + 26);
            assert!(id.starts_with(prefix));
        }
    }

    #[test]
    fn claim_gate_failures_map_onto_the_occupancy_taxonomy() {
        let access = claim_gate_error(ClientOccupancyServiceErrorKind::AccessDenied);
        assert_eq!(access.kind(), ClientOccupancyErrorKind::AccessDenied);
        let offline = claim_gate_error(ClientOccupancyServiceErrorKind::PresenceNotOnline);
        assert_eq!(offline.kind(), ClientOccupancyErrorKind::ClientOffline);
        let locked = claim_gate_error(ClientOccupancyServiceErrorKind::ClientLocked);
        assert_eq!(locked.kind(), ClientOccupancyErrorKind::ClientLocked);
        let capacity = claim_gate_error(ClientOccupancyServiceErrorKind::CapacityExhausted);
        assert_eq!(capacity.kind(), ClientOccupancyErrorKind::CapacityExhausted);
        let occupied = claim_gate_error(ClientOccupancyServiceErrorKind::ActiveLeaseConflict);
        assert_eq!(occupied.kind(), ClientOccupancyErrorKind::OccupiedByOther);
        let storage = claim_gate_error(ClientOccupancyServiceErrorKind::Storage);
        assert_eq!(storage.kind(), ClientOccupancyErrorKind::Unavailable);
        assert!(claim_gate_failure_is_throttled(
            ClientOccupancyServiceErrorKind::ActiveLeaseConflict
        ));
        assert!(!claim_gate_failure_is_throttled(
            ClientOccupancyServiceErrorKind::Storage
        ));
    }
}
