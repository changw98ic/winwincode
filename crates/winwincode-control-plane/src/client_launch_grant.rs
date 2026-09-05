// SPDX-License-Identifier: Apache-2.0

//! `WorkerLaunchGrant` application service over the durable Server-side
//! launch grant ledger.
//!
//! The Control Plane is the authoritative owner of Worker launch grants (plan
//! 7.8, 14, 17.2): it issues one grant per worker launch behind the
//! five-condition issue gate (device online, lease holder with a confirmed
//! `occupied` or `draining` lease and the exact fencing token, a binding
//! belonging to the leased client and visible to the holder, a free
//! worker-session slot), settles the Device Client's
//! `client.worker.launch_ack` exactly once, and revokes or expires grants
//! through their frozen `issued | consumed | revoked | expired` state
//! machine. Every transition lands in the durable launch audit trail.

use std::fmt;

use winwincode_domain::Instant;
use winwincode_storage::{
    LaunchAckOutcome, LaunchAckSettlement, LaunchAuditEntry, LaunchGrantIssuance, SqliteStorage,
    WorkerLaunchGrantStoreError, WorkerLaunchGrantStoreErrorKind,
};

/// Re-exported so service consumers can name the frozen grant states and
/// records without importing the storage crate directly.
pub use winwincode_storage::WorkerLaunchGrantRecord;
pub use winwincode_storage::WorkerLaunchGrantState as LaunchGrantState;

/// Stable service failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerLaunchGrantServiceErrorKind {
    /// A command input violated the frozen schema bounds.
    InvalidInput,
    /// The client node identity does not exist.
    UnknownClientNode,
    /// The client node presence is not `online`.
    PresenceNotOnline,
    /// The client node is `locked`.
    ClientLocked,
    /// No occupancy lease matches the requested identity.
    UnknownOccupancyLease,
    /// The lease belongs to a user other than the grant holder.
    NotLeaseHolder,
    /// The lease is neither `occupied` nor `draining`.
    OccupancyNotConfirmed,
    /// The command carried a fencing token other than the bound token.
    FencingTokenMismatch,
    /// No repository binding matches the requested identity.
    UnknownRepositoryBinding,
    /// The binding belongs to a client node other than the leased one.
    BindingForeignClient,
    /// The binding is not visible to the holder.
    BindingNotVisible,
    /// The client node has no free worker-session slot.
    CapacityExhausted,
    /// The worker session already carries a non-terminal grant.
    LaunchGrantConflict,
    /// The grant's expiry deadline passed before the acknowledgement.
    GrantExpired,
    /// No launch grant matches the requested identity.
    UnknownLaunchGrant,
    /// An echoed settlement field does not match the grant binding.
    FieldMismatch,
    /// The requested change is not a legal state machine transition.
    IllegalStateTransition,
    /// A compare-and-swap guard lost an impossible race.
    RevisionConflict,
    /// A durable row violated the frozen schema invariants.
    CorruptState,
    /// The underlying storage operation failed.
    Storage,
}

/// Secret-free launch grant service error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerLaunchGrantServiceError {
    kind: WorkerLaunchGrantServiceErrorKind,
    message: String,
}

impl WorkerLaunchGrantServiceError {
    #[must_use]
    pub const fn kind(&self) -> WorkerLaunchGrantServiceErrorKind {
        self.kind
    }
}

impl fmt::Display for WorkerLaunchGrantServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for WorkerLaunchGrantServiceError {}

impl From<WorkerLaunchGrantStoreError> for WorkerLaunchGrantServiceError {
    fn from(source: WorkerLaunchGrantStoreError) -> Self {
        Self {
            kind: match source.kind() {
                WorkerLaunchGrantStoreErrorKind::InvalidInput => {
                    WorkerLaunchGrantServiceErrorKind::InvalidInput
                }
                WorkerLaunchGrantStoreErrorKind::UnknownClientNode => {
                    WorkerLaunchGrantServiceErrorKind::UnknownClientNode
                }
                WorkerLaunchGrantStoreErrorKind::PresenceNotOnline => {
                    WorkerLaunchGrantServiceErrorKind::PresenceNotOnline
                }
                WorkerLaunchGrantStoreErrorKind::ClientLocked => {
                    WorkerLaunchGrantServiceErrorKind::ClientLocked
                }
                WorkerLaunchGrantStoreErrorKind::UnknownOccupancyLease => {
                    WorkerLaunchGrantServiceErrorKind::UnknownOccupancyLease
                }
                WorkerLaunchGrantStoreErrorKind::NotLeaseHolder => {
                    WorkerLaunchGrantServiceErrorKind::NotLeaseHolder
                }
                WorkerLaunchGrantStoreErrorKind::OccupancyNotConfirmed => {
                    WorkerLaunchGrantServiceErrorKind::OccupancyNotConfirmed
                }
                WorkerLaunchGrantStoreErrorKind::FencingTokenMismatch => {
                    WorkerLaunchGrantServiceErrorKind::FencingTokenMismatch
                }
                WorkerLaunchGrantStoreErrorKind::UnknownRepositoryBinding => {
                    WorkerLaunchGrantServiceErrorKind::UnknownRepositoryBinding
                }
                WorkerLaunchGrantStoreErrorKind::BindingForeignClient => {
                    WorkerLaunchGrantServiceErrorKind::BindingForeignClient
                }
                WorkerLaunchGrantStoreErrorKind::BindingNotVisible => {
                    WorkerLaunchGrantServiceErrorKind::BindingNotVisible
                }
                WorkerLaunchGrantStoreErrorKind::CapacityExhausted => {
                    WorkerLaunchGrantServiceErrorKind::CapacityExhausted
                }
                WorkerLaunchGrantStoreErrorKind::LaunchGrantConflict => {
                    WorkerLaunchGrantServiceErrorKind::LaunchGrantConflict
                }
                WorkerLaunchGrantStoreErrorKind::GrantExpired => {
                    WorkerLaunchGrantServiceErrorKind::GrantExpired
                }
                WorkerLaunchGrantStoreErrorKind::UnknownLaunchGrant => {
                    WorkerLaunchGrantServiceErrorKind::UnknownLaunchGrant
                }
                WorkerLaunchGrantStoreErrorKind::FieldMismatch => {
                    WorkerLaunchGrantServiceErrorKind::FieldMismatch
                }
                WorkerLaunchGrantStoreErrorKind::IllegalStateTransition => {
                    WorkerLaunchGrantServiceErrorKind::IllegalStateTransition
                }
                WorkerLaunchGrantStoreErrorKind::RevisionConflict => {
                    WorkerLaunchGrantServiceErrorKind::RevisionConflict
                }
                WorkerLaunchGrantStoreErrorKind::CorruptState => {
                    WorkerLaunchGrantServiceErrorKind::CorruptState
                }
                WorkerLaunchGrantStoreErrorKind::Storage => {
                    WorkerLaunchGrantServiceErrorKind::Storage
                }
            },
            message: source.to_string(),
        }
    }
}

/// Worker launch grant application service over one storage connection.
pub struct WorkerLaunchGrantService<'storage> {
    storage: &'storage mut SqliteStorage,
}

impl<'storage> WorkerLaunchGrantService<'storage> {
    /// Builds one service over the sole product-state storage authority.
    #[must_use]
    pub fn new(storage: &'storage mut SqliteStorage) -> Self {
        Self { storage }
    }

    /// Atomically issues one worker launch grant behind the issue gate (plan
    /// 14.3 step 4). The `issued` grant and its audit row commit together.
    ///
    /// # Errors
    ///
    /// Rejects any failed gate condition, a reused grant id, or storage
    /// failure.
    pub fn issue(
        &mut self,
        issuance: &LaunchGrantIssuance,
        now: &Instant,
    ) -> Result<WorkerLaunchGrantRecord, WorkerLaunchGrantServiceError> {
        Ok(self
            .storage
            .worker_launch_grant_ledger()?
            .issue(issuance, now)?)
    }

    /// Settles one `client.worker.launch_ack` exactly once (plan 14.3 step
    /// 10): an accepted acknowledgement consumes an `issued` grant, a replay
    /// of a consumed grant is an idempotent no-op, and a rejection keeps the
    /// grant `issued` with the reason recorded in the audit trail.
    ///
    /// # Errors
    ///
    /// Rejects an unknown grant, a field mismatch, a stale token, an expired
    /// grant, an illegal transition, or storage failure.
    pub fn settle_launch_ack(
        &mut self,
        settlement: &LaunchAckSettlement,
        now: &Instant,
    ) -> Result<LaunchAckOutcome, WorkerLaunchGrantServiceError> {
        Ok(self
            .storage
            .worker_launch_grant_ledger()?
            .settle_launch_ack(settlement, now)?)
    }

    /// Revokes an `issued` grant before the device accepted it.
    ///
    /// # Errors
    ///
    /// Rejects an unknown grant, a non-`issued` grant, or storage failure.
    pub fn revoke(
        &mut self,
        worker_launch_grant_id: &str,
        actor_user_id: &str,
        reason: Option<&str>,
        now: &Instant,
    ) -> Result<WorkerLaunchGrantRecord, WorkerLaunchGrantServiceError> {
        Ok(self.storage.worker_launch_grant_ledger()?.revoke(
            worker_launch_grant_id,
            actor_user_id,
            reason,
            now,
        )?)
    }

    /// Expires every `issued` grant whose expiry deadline passed; returns
    /// the expired grant ids.
    ///
    /// # Errors
    ///
    /// Rejects an invalid cutoff or storage failure.
    pub fn expire(
        &mut self,
        cutoff: &Instant,
    ) -> Result<Vec<String>, WorkerLaunchGrantServiceError> {
        Ok(self.storage.worker_launch_grant_ledger()?.expire(cutoff)?)
    }

    /// Returns one durable launch grant projection.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical grant identity or storage failure.
    pub fn snapshot(
        &mut self,
        worker_launch_grant_id: &str,
    ) -> Result<Option<WorkerLaunchGrantRecord>, WorkerLaunchGrantServiceError> {
        Ok(self
            .storage
            .worker_launch_grant_ledger()?
            .snapshot(worker_launch_grant_id)?)
    }

    /// Returns the one non-terminal grant of a worker session, if any.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical worker session identity or storage failure.
    pub fn active_grant_for_session(
        &mut self,
        worker_session_id: &str,
    ) -> Result<Option<WorkerLaunchGrantRecord>, WorkerLaunchGrantServiceError> {
        Ok(self
            .storage
            .worker_launch_grant_ledger()?
            .active_grant_for_session(worker_session_id)?)
    }

    /// Returns the newest launch grant anchored to one product session, if
    /// any, whatever its lifecycle state: the anchor is the permission fact
    /// that ties the session to device execution.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical product session identity or storage failure.
    pub fn newest_grant_for_product_session(
        &mut self,
        product_session_id: &str,
    ) -> Result<Option<WorkerLaunchGrantRecord>, WorkerLaunchGrantServiceError> {
        Ok(self
            .storage
            .worker_launch_grant_ledger()?
            .newest_grant_for_product_session(product_session_id)?)
    }

    /// Counts the non-terminal grants of one client node — the durable
    /// reservation view capacity is judged against (plan 14.5).
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical client node identity or storage failure.
    pub fn non_terminal_count_for_node(
        &mut self,
        client_node_id: &str,
    ) -> Result<u64, WorkerLaunchGrantServiceError> {
        Ok(self
            .storage
            .worker_launch_grant_ledger()?
            .non_terminal_count_for_node(client_node_id)?)
    }

    /// Returns the durable launch audit trail of one grant, oldest first.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical grant identity or storage failure.
    pub fn audit_trail(
        &mut self,
        worker_launch_grant_id: &str,
    ) -> Result<Vec<LaunchAuditEntry>, WorkerLaunchGrantServiceError> {
        Ok(self
            .storage
            .worker_launch_grant_ledger()?
            .audit_trail(worker_launch_grant_id)?)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use winwincode_storage::{
        AccessGrantIssuance, ClientNodeRegistration, ClientPresenceState, GrantPermissions,
        GrantSource, GrantTrustMode, OccupancyClaim, OccupancyLeaseState,
        RepositoryAccessGrantIssuance, RepositoryAvailability, RepositoryBindingProjection,
        RepositoryDirtyState, RepositoryGrantPermissions,
    };

    use super::*;

    static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

    fn temporary_directory(name: &str) -> PathBuf {
        let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "winwincode-launch-grant-service-{name}-{}-{suffix}-{nanos}",
            std::process::id()
        ))
    }

    fn instant(value: &str) -> Instant {
        Instant(value.to_owned())
    }

    fn suffix(seed: u64) -> String {
        const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
        let mut identity = String::with_capacity(26);
        let mut value = seed;
        for _ in 0..26 {
            let digit = usize::try_from(value % 32).expect("digit fits");
            identity.push(ALPHABET[digit] as char);
            value /= 32;
        }
        identity
    }

    const DIGEST: &str = "sha256:00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    /// Seeds the happy-path fixture and returns every identity the issuance
    /// command needs.
    fn seed_fixture(storage: &mut SqliteStorage, seed: u64) -> (String, String, u64, String) {
        let node = format!("cnd_{}", suffix(seed));
        let holder = format!("usr_{}", suffix(seed + 1));
        {
            let registration = ClientNodeRegistration::try_new(
                node.clone(),
                format!("{seed:010}"),
                "Service Test Device".to_owned(),
                "aarch64-apple-darwin",
                "aarch64",
                "1.2.3",
                None,
                Some(format!("cix_{}", suffix(seed + 2))),
                4,
            )
            .expect("registration");
            let mut registry = storage.client_node_registry().expect("registry");
            registry
                .register(&registration, 0, &instant("2026-01-01T00:00:00.000Z"))
                .expect("register");
            registry
                .update_presence(&node, ClientPresenceState::Online, 1)
                .expect("presence");
        }
        {
            let issuance = AccessGrantIssuance::try_new(
                format!("cag_{}", suffix(seed + 3)),
                &node,
                &holder,
                &holder,
                GrantTrustMode::Trusted,
                None,
            )
            .expect("issuance");
            let mut ledger = storage.client_connect_ledger().expect("ledger");
            ledger
                .create_grant(
                    &issuance,
                    GrantSource::Administrator,
                    GrantPermissions::USE,
                    &instant("2026-01-01T00:00:10.000Z"),
                )
                .expect("grant");
        }
        let (lease_id, token) = {
            let mut occupancy = storage.client_occupancy_ledger().expect("ledger");
            let claim = OccupancyClaim::try_new(
                format!("ocl_{}", suffix(seed + 4)),
                &node,
                &holder,
                format!("req_{}", suffix(seed + 5)),
            )
            .expect("claim");
            let lease = occupancy
                .atomic_claim(&claim, &instant("2026-01-01T00:01:00.000Z"))
                .expect("claim");
            let occupied = occupancy
                .record_acknowledgement(
                    &lease.occupancy_lease_id,
                    lease.fencing_token,
                    None,
                    &instant("2026-01-01T00:01:01.000Z"),
                )
                .expect("ack");
            assert_eq!(occupied.state, OccupancyLeaseState::Occupied);
            (occupied.occupancy_lease_id, occupied.fencing_token)
        };
        let binding = format!("rbd_{}", suffix(seed + 6));
        {
            let mut ledger = storage.repository_binding_ledger().expect("ledger");
            let projection = RepositoryBindingProjection::try_new(
                binding.clone(),
                &node,
                "winwincode",
                Some("main".to_owned()),
                Some("0123456789abcdef0123456789abcdef01234567".to_owned()),
                RepositoryDirtyState::Clean,
                RepositoryAvailability::Available,
                format!("sha256:{seed:064}"),
            )
            .expect("projection");
            ledger
                .upsert(&projection, None, 0, &instant("2026-01-01T00:00:30.000Z"))
                .expect("upsert");
            let issuance = RepositoryAccessGrantIssuance::try_new(
                format!("rag_{}", suffix(seed + 7)),
                &binding,
                &holder,
                &holder,
            )
            .expect("repo issuance");
            ledger
                .create_grant(
                    &issuance,
                    RepositoryGrantPermissions::Use,
                    &instant("2026-01-01T00:00:31.000Z"),
                )
                .expect("repo grant");
        }
        (node, lease_id, token, binding)
    }

    fn issuance_for(
        seed: u64,
        node: &str,
        holder: &str,
        lease_id: &str,
        token: u64,
        binding: &str,
    ) -> LaunchGrantIssuance {
        LaunchGrantIssuance::try_new(
            format!("wlg_{}", suffix(seed)),
            node,
            format!("cix_{}", suffix(seed + 2)),
            holder,
            lease_id,
            token,
            binding,
            format!("ws_{}", suffix(seed + 8)),
            format!("wkr_{}", suffix(seed + 9)),
            format!("winst_{}", suffix(seed + 10)),
            DIGEST,
            Some(format!("ps_{}", suffix(seed + 11))),
            Some(format!("run_{}", suffix(seed + 12))),
            instant("2026-01-01T01:00:00.000Z"),
        )
        .expect("issuance")
    }

    fn settlement_for(
        seed: u64,
        lease_id: &str,
        token: u64,
        accepted: bool,
    ) -> LaunchAckSettlement {
        LaunchAckSettlement::try_new(
            format!("wlg_{}", suffix(seed)),
            lease_id,
            token,
            format!("ws_{}", suffix(seed + 8)),
            format!("wkr_{}", suffix(seed + 9)),
            format!("winst_{}", suffix(seed + 10)),
            accepted,
            (!accepted).then(|| "rejected_capacity_exhausted".to_owned()),
        )
        .expect("settlement")
    }

    #[test]
    fn representative_store_errors_map_one_to_one_onto_the_service_taxonomy() {
        // Input validation failures need no database.
        let invalid = LaunchGrantIssuance::try_new(
            "nope",
            format!("cnd_{}", suffix(1)),
            format!("cix_{}", suffix(2)),
            format!("usr_{}", suffix(3)),
            format!("ocl_{}", suffix(4)),
            1,
            format!("rbd_{}", suffix(5)),
            format!("ws_{}", suffix(6)),
            format!("wkr_{}", suffix(7)),
            format!("winst_{}", suffix(8)),
            DIGEST,
            None,
            None,
            instant("2026-01-01T01:00:00.000Z"),
        )
        .expect_err("a non-canonical grant id is invalid input");
        assert_eq!(
            WorkerLaunchGrantServiceError::from(invalid).kind(),
            WorkerLaunchGrantServiceErrorKind::InvalidInput
        );

        let mut storage = SqliteStorage::open(temporary_directory("mapping")).expect("storage");
        // Production flows open the registry ledger first; touch it so the
        // client_nodes schema exists on this fresh database.
        let _ = storage.client_node_registry().expect("registry");
        let mut service = WorkerLaunchGrantService::new(&mut storage);
        // Issue against an empty database names the unknown client node.
        let issuance = LaunchGrantIssuance::try_new(
            format!("wlg_{}", suffix(20)),
            format!("cnd_{}", suffix(21)),
            format!("cix_{}", suffix(22)),
            format!("usr_{}", suffix(23)),
            format!("ocl_{}", suffix(24)),
            1,
            format!("rbd_{}", suffix(25)),
            format!("ws_{}", suffix(26)),
            format!("wkr_{}", suffix(27)),
            format!("winst_{}", suffix(28)),
            DIGEST,
            None,
            None,
            instant("2026-01-01T01:00:00.000Z"),
        )
        .expect("issuance");
        let error = service
            .issue(&issuance, &instant("2026-01-01T00:02:00.000Z"))
            .expect_err("an unknown node must be refused");
        assert_eq!(
            error.kind(),
            WorkerLaunchGrantServiceErrorKind::UnknownClientNode
        );
        // Settlement and revocation against an unknown grant name the
        // unknown-grant category.
        let settlement = LaunchAckSettlement::try_new(
            format!("wlg_{}", suffix(30)),
            format!("ocl_{}", suffix(31)),
            1,
            format!("ws_{}", suffix(32)),
            format!("wkr_{}", suffix(33)),
            format!("winst_{}", suffix(34)),
            true,
            None,
        )
        .expect("settlement");
        let error = service
            .settle_launch_ack(&settlement, &instant("2026-01-01T00:03:00.000Z"))
            .expect_err("an unknown grant must be refused");
        assert_eq!(
            error.kind(),
            WorkerLaunchGrantServiceErrorKind::UnknownLaunchGrant
        );
        let error = service
            .revoke(
                &format!("wlg_{}", suffix(30)),
                &format!("usr_{}", suffix(23)),
                None,
                &instant("2026-01-01T00:03:01.000Z"),
            )
            .expect_err("an unknown grant must be refused");
        assert_eq!(
            error.kind(),
            WorkerLaunchGrantServiceErrorKind::UnknownLaunchGrant
        );
        assert!(
            service
                .snapshot(&format!("wlg_{}", suffix(30)))
                .expect("snapshot")
                .is_none()
        );
    }

    #[test]
    fn the_service_drives_issue_ack_and_audit_end_to_end() {
        let mut storage = SqliteStorage::open(temporary_directory("happy")).expect("storage");
        let seed = 200;
        let (node, lease_id, token, binding) = seed_fixture(&mut storage, seed);
        let holder = format!("usr_{}", suffix(seed + 1));
        let issuance = issuance_for(seed + 20, &node, &holder, &lease_id, token, &binding);
        let grant_id = issuance.worker_launch_grant_id().to_owned();
        {
            let mut service = WorkerLaunchGrantService::new(&mut storage);
            let issued = service
                .issue(&issuance, &instant("2026-01-01T00:02:00.000Z"))
                .expect("issue");
            assert_eq!(issued.state.as_str(), "issued");
            assert_eq!(
                service.non_terminal_count_for_node(&node).expect("count"),
                1
            );
        }
        {
            let mut service = WorkerLaunchGrantService::new(&mut storage);
            let outcome = service
                .settle_launch_ack(
                    &settlement_for(seed + 20, &lease_id, token, true),
                    &instant("2026-01-01T00:03:00.000Z"),
                )
                .expect("settle");
            assert!(matches!(outcome, LaunchAckOutcome::Consumed(_)));
            // The replay stays an accepted idempotent no-op.
            let outcome = service
                .settle_launch_ack(
                    &settlement_for(seed + 20, &lease_id, token, true),
                    &instant("2026-01-01T00:03:01.000Z"),
                )
                .expect("replay");
            assert_eq!(outcome, LaunchAckOutcome::AlreadyConsumed);
        }
        {
            let mut service = WorkerLaunchGrantService::new(&mut storage);
            let grant = service
                .snapshot(&grant_id)
                .expect("snapshot")
                .expect("grant");
            assert_eq!(grant.state.as_str(), "consumed");
            let session_grant = service
                .active_grant_for_session(&grant.worker_session_id)
                .expect("session grant")
                .expect("live grant");
            assert_eq!(
                session_grant.worker_launch_grant_id,
                grant.worker_launch_grant_id
            );
            let trail = service.audit_trail(&grant_id).expect("trail");
            let actions = trail
                .iter()
                .map(|entry| entry.action.as_str())
                .collect::<Vec<_>>();
            assert_eq!(actions, vec!["issued", "consumed"]);
        }
    }

    #[test]
    fn the_service_surfaces_the_gate_rejections_for_the_boundary() {
        let mut storage = SqliteStorage::open(temporary_directory("gate")).expect("storage");
        let seed = 300;
        let (node, lease_id, token, binding) = seed_fixture(&mut storage, seed);
        let holder = format!("usr_{}", suffix(seed + 1));
        let outsider = format!("usr_{}", suffix(seed + 13));
        let mut service = WorkerLaunchGrantService::new(&mut storage);
        let error = service
            .issue(
                &issuance_for(seed + 20, &node, &outsider, &lease_id, token, &binding),
                &instant("2026-01-01T00:02:00.000Z"),
            )
            .expect_err("a foreign holder must be refused");
        assert_eq!(
            error.kind(),
            WorkerLaunchGrantServiceErrorKind::NotLeaseHolder
        );
        let error = service
            .issue(
                &issuance_for(seed + 21, &node, &holder, &lease_id, token + 1, &binding),
                &instant("2026-01-01T00:02:01.000Z"),
            )
            .expect_err("a stale token must be refused");
        assert_eq!(
            error.kind(),
            WorkerLaunchGrantServiceErrorKind::FencingTokenMismatch
        );
        let error = service
            .issue(
                &issuance_for(
                    seed + 22,
                    &format!("cnd_{}", suffix(999)),
                    &holder,
                    &lease_id,
                    token,
                    &binding,
                ),
                &instant("2026-01-01T00:02:02.000Z"),
            )
            .expect_err("an unknown node must be refused");
        assert_eq!(
            error.kind(),
            WorkerLaunchGrantServiceErrorKind::UnknownClientNode
        );
    }
}
