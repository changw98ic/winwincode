#![allow(clippy::doc_markdown)]
// SPDX-License-Identifier: Apache-2.0

use time::{OffsetDateTime, UtcOffset};

pub(crate) fn canonical_rfc3339(time: OffsetDateTime) -> String {
    let time = time.to_offset(UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        time.year(),
        u8::from(time.month()),
        time.day(),
        time.hour(),
        time.minute(),
        time.second(),
        time.nanosecond() / 1_000_000,
    )
}

mod application_journal;
pub mod apply_engine;
pub mod candidate_branch;
pub mod candidate_registry;
pub mod candidate_retention;
pub mod connect_code;
pub mod daemon;
pub mod fencing;
pub mod http;
pub mod identity;
pub mod managed_app;
pub mod path_confinement;
pub mod preview;
pub mod remote_auth;
pub mod repository;
pub mod repository_exchange;
pub mod repository_git;
pub mod service;
pub mod store;
pub mod supervisor;
pub mod worker_logs;

pub use apply_engine::{
    CandidateApplyError, CandidateApplyErrorKind, CandidateApplyOutcome, CandidateApplyRequest,
    CandidateIntegrationConflict, CandidateIntegrationOutcome, CandidateIntegrationRequest,
    CandidateRecoveryGuidance, GitRecoveryCommand, INTEGRATION_REF_PREFIX,
    PreparedCandidateIntegration, RecoveryCommandDirectory, apply_candidate_to_branch,
    candidate_recovery_guidance, integrate_candidates_for_verification,
};
pub use candidate_branch::{
    BranchCreationFacts, BranchCreationOutcome, BranchCreationReport, CandidateBranchError,
    CandidateBranchErrorKind, CreatedBranchRecord, WINWINCODE_BRANCH_PREFIX,
    create_candidate_branch, created_branch_record, enqueue_branch_created,
};
pub use candidate_registry::{
    CANDIDATE_REF_PREFIX, CandidateLocalRefRecord, CandidateReconciliation, CandidateRefVerdict,
    CandidateRegistryError, CandidateRegistryErrorKind, CandidateRetainReport, CandidateRetention,
    CandidateRetentionOutcome, candidate_local_ref, enqueue_candidate_retained,
    progress_candidate_lifecycle, reconcile_retained_candidates, record_candidate_retention,
    retain_candidate, retained_candidates,
};
pub use candidate_retention::{
    CandidateDiscardFacts, CandidateDiscardOutcome, CandidateDiscardRecord, CandidateDiscardReport,
    CandidateDiscardRequest, CandidateRetentionError, CandidateRetentionErrorKind,
    CandidateRetentionPolicy, CandidateUplinkState, CollectedCandidate, DeferredCandidate,
    GcDeferralReason, GcReport, RetentionSweepReport, candidate_discard_record,
    collect_expired_candidates, discard_candidate, enforce_retention_policy,
    enqueue_candidate_discarded,
};
pub use connect_code::{
    CONNECT_CODE_DIGITS, CONNECT_CODE_TTL, ConnectCodeError, ConnectCodePlaintext,
    PublishedConnectCode,
};
pub use daemon::{
    DaemonConfig, DaemonError, DaemonStatus, DeviceDaemon, EnrollmentIssuance, ExchangeRequest,
    ExchangeResponse, ExchangeTransport, ExchangeTransportError, LeaseWorkerController,
    ManagedAppRepositorySource, TickOutcome, WorkerCapacitySnapshot, WorkerCapacitySource,
    WorkerLaunchDirectories, WorkerLaunchMaterialSource,
};
pub use fencing::{
    FencedCommandKind, FencingGuard, FencingRejection, FencingTicket, FencingVerdict,
};
pub use http::HttpExchangeTransport;
pub use identity::{
    DeviceIdentity, DeviceIdentitySeed, IdentityRecord, IssuedEnrollment, adopt_enrollment,
    ensure_device_identity, load_device_identity,
};
pub use managed_app::{ManagedAppError, ManagedAppSupervisor, ManagedAppSupervisorConfig};
pub use path_confinement::{ConfinedPath, ConfinedRoot, ConfinementVerdict, PathConfinementError};
pub use preview::{
    AuthorizedPreviewSource, AuthorizedPreviewSourceRegistry, PreviewTunnelClient,
    PreviewTunnelError, PreviewTunnelSupervisor,
};
pub use remote_auth::{
    DeviceBindConfig, DeviceBindOutcome, DeviceRemoteAuth, bind_device, load_ca_roots,
    load_remote_binding,
};
pub use repository::{
    RegistrationOptions, RegistrationRejection, RepositoryBindingSummary, RepositoryRegistration,
    RepositoryRegistryError, RepositoryRemoval, RepositoryRevalidation, list_bindings,
    register_repository, remove_repository, repository_fingerprint, revalidate_repository,
};
pub use repository_exchange::{RepositoryRescanApplication, apply_repository_rescan};
pub use repository_git::{
    DETACHED_BRANCH, GitHeadState, GitInspectError, GitInspectOptions, GitInspector, GitScan,
};
pub use service::{
    DeviceServiceConfig, DeviceServiceError, DeviceServiceStatus, device_service_logs,
    device_service_status, request_device_service_restart, run_device_service,
    run_device_service_with_ca,
};
pub use store::{
    CLIENT_STORE_SCHEMA_VERSION, ClientInboxCursor, ClientInboxCursorUpdate, ClientOutboxEntry,
    ConnectCodeStateRecord, ConnectionPolicyRecord, DeviceStore, DeviceStoreError,
    DeviceStoreErrorKind, OccupancyMirrorAdvance, OccupancyMirrorRecord, OccupancyMirrorUpdate,
    OccupancyReleaseIntentOutcome, OccupancyReleaseIntentRecord, PathMappingRecord,
    RepositoryLocalStateRecord, ServerProfileRecord, WorkerProcessRecord, WorkerStopIntentOutcome,
    WorkerStopIntentRecord, availability_wire_name, dirty_state_wire_name,
};
pub use supervisor::{
    ModelRoute, ReapedWorker, SessionSupervisor, SpawnOutcome, SpawnRequest, SupervisorConfig,
    SupervisorError, WORKER_STATE_CRASHED, WORKER_STATE_EXITED, WORKER_STATE_MISSING,
    WORKER_STATE_RUNNING, WorkerHandle, WorkerReconcileReport, WorkerReconcileVerdict,
    WorkerStopOutcome,
};
pub use worker_logs::{
    WORKER_LOG_SCHEMA_VERSION, WorkerExitFact, WorkerExitRecordOutcome, WorkerLogAppendStats,
    WorkerLogConfig, WorkerLogContentKind, WorkerLogCrashReference, WorkerLogError,
    WorkerLogRecorder, WorkerLogStream, WorkerLogSummary,
};

pub use winwincode_client_port::domain::{ClientLockState, ConnectCodeState};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_timestamps_are_utc_with_millisecond_precision() {
        let time = OffsetDateTime::parse(
            "2026-09-12T11:09:20.050338+08:00",
            &time::format_description::well_known::Rfc3339,
        )
        .expect("fixture timestamp parses");

        assert_eq!(canonical_rfc3339(time), "2026-09-12T03:09:20.050Z");
    }
}
