// SPDX-License-Identifier: Apache-2.0

//! Local device-client data for the multi-user shared `WinWinCode` device
//! client (plan section 8).
//!
//! This crate owns the purely local concerns of a device client:
//!
//! - [`store`]: the on-device `SQLite` database holding the eleven local
//!   tables from plan section 8 plus the two CLIENT-200.2 tables
//!   (`connect_code_state`, `client_connection_policy`), including
//!   the durable client-to-server outbox adapter implementing the canonical
//!   `winwincode-client-port` exchange traits ([`FrameOutbox`] and
//!   [`CompactingOutbox`]).
//! - [`identity`]: first-boot generation and durable persistence of the
//!   device identity and device credential, including the fresh-per-launch
//!   canonical `clientInstanceId` and the server-issued enrollment identity
//!   adopted after the exchange (`adopt_enrollment`).
//! - [`connect_code`]: the dynamic connect code lifecycle (CLIENT-200.2,
//!   plan 11.1/11.3) — strong 8-digit code generation, 120-second
//!   publications, refresh-superseded generations, the local connection
//!   policy (lock / new connections), challenge verdicts, and the durable
//!   `client.connect_code.published` frame.
//! - [`repository`]: the local repository registry (plan 8.1, 13.1–13.3,
//!   13.5) — the registration check chain (canonicalize with symlink
//!   resolution and replacement detection, readable directory,
//!   confirm-or-initialize Git, common directory, HEAD/branch/dirty), random
//!   `rbd_` binding ids, the local path mapping and scan projection, the
//!   path-free `client.repository.upsert` / `removed` / `status` frames, and
//!   the launch-time [`repository::revalidate_repository`] the Worker epic
//!   must call before every launch.
//! - [`repository_git`]: the independent Git inspector behind the registry —
//!   one `git`-shelling probe surface that classifies repository shapes
//!   stably (plain, dirty, detached HEAD, unborn branch, linked worktree,
//!   submodule, bare, non-Git) and owns the documented
//!   `sha256(head \0 branch)` repository fingerprint rule.
//! - [`path_confinement`]: fail-closed containment of canonical paths within
//!   an authorized binding root — component-boundary exact, symlink- and
//!   `..`-refusing, reading nothing outside the root — the primitive the
//!   launch-time revalidation reuses.
//! - [`fencing`]: the pure occupancy fencing decision surface
//!   (CLIENT-300.3, plan 12.6) — [`FencingGuard::authorize_command`] over
//!   the four fenced command entry points (worker launch/stop, candidate
//!   apply, repository mutation), with revision-bound tickets that a mirror
//!   advance invalidates.
//! - [`candidate_registry`]: the device-local candidate registry
//!   (GIT-100.2, plan 7.9/15.2) — durable retention of frozen Worker
//!   candidates (`candidate_local_refs`), the lease-stamped durable
//!   `client.candidate.retained` uplink, restart recovery of the retained
//!   set, and the read-only reconciliation of retained candidates against
//!   the actual Git refs of their bound checkouts.
//! - [`apply_engine`]: the target-branch safe apply engine (GIT-100.4, plan
//!   15.4/15.5) — preflight (candidate ref, `expectedHead` equality, dirty
//!   policy, occupancy fencing), strategy execution in an isolated
//!   integration worktree that never touches the user's working tree, the
//!   compare-and-swap atomic target ref update, kept conflict artifacts, and
//!   one durable `client.candidate.apply_result` receipt per attempt.
//! - [`candidate_retention`]: the candidate retention policy, discard, and
//!   garbage collection (GIT-100.9, plan 15) — the configurable per-binding
//!   cap on active candidates with oldest-first auto-discard, the fail-closed
//!   discard vertical (created-branch deletion, `discarded` registry
//!   transition, lease-stamped `client.candidate.apply_result` uplink), and
//!   the crash-resumable GC that reclaims a terminal candidate's stable ref
//!   only after its retention window and a fully acknowledged uplink.
//! - [`daemon`]: the periodic device-client exchange loop
//!   (`POST /internal/v1/client/exchange`) over an injected transport:
//!   enrollment adoption, hello announcement, heartbeat reporting,
//!   acknowledgement advancement, gap replay, access-challenge answering,
//!   client-lock application, occupancy mirroring (offer → durable mirror →
//!   ack, release intents, force-fence overwrites), and exponential-backoff
//!   recovery on a plain `std` thread — no async runtime.
//! - [`supervisor`]: the local one-session-one-worker process supervisor
//!   (WORKER-100.2, plan 14.1/14.5/8.2/18.3) — fencing-gated managed
//!   worker spawns over 0600 config/credential files, the durable
//!   `worker_process_registry` (pid + process-start identity), reap/stop
//!   with terminal observations, the restart `reconcile` scan, and the
//!   live capacity facts wired into the daemon's hello/heartbeat.
//! - [`worker_logs`]: the bounded, redacted worker-subprocess logs
//!   (WORKER-100.5) — capped, rotated, retention-pruned stdout/stderr
//!   capture filtered at ingest (credentials, absolute paths, model
//!   bodies), idempotent terminal exit facts, and the safe diagnostics
//!   surface ([`worker_logs::WorkerLogSummary`] and
//!   [`worker_logs::WorkerLogCrashReference`]) that correlates an abnormal
//!   exit to its unique WorkerSession without ever exposing raw content.
//! - [`http`]: the dependency-free std TCP HTTP/1.1 exchange transport
//!   implementation of [`daemon::ExchangeTransport`].
//!
//! The local database never leaves the device: absolute paths stored in
//! `repository_path_mapping` (and worker/candidate data directories) are
//! never uploaded to any server; the candidate registry reconciles against
//! Git through those local paths and reports only stable identities.
//!
//! The SQLite storage patterns (open sequence, migration style, transaction
//! discipline, static-SQL-only rule, and test infrastructure) deliberately
//! mirror `crates/winwincode-storage`, the authoritative local-SQLite
//! storage adapter in this repository.
//!
//! [`FrameOutbox`]: winwincode_client_port::exchange::FrameOutbox
//! [`CompactingOutbox`]: winwincode_client_port::exchange::CompactingOutbox

#![allow(clippy::doc_markdown)]

pub mod apply_engine;
pub mod candidate_branch;
pub mod candidate_registry;
pub mod candidate_retention;
pub mod connect_code;
pub mod daemon;
pub mod fencing;
pub mod http;
pub mod identity;
pub mod path_confinement;
pub mod repository;
pub mod repository_git;
pub mod store;
pub mod supervisor;
pub mod worker_logs;

pub use apply_engine::{
    CandidateApplyError, CandidateApplyErrorKind, CandidateApplyOutcome, CandidateApplyRequest,
    apply_candidate_to_branch,
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
    CONNECT_CODE_DIGITS, CONNECT_CODE_TTL, ChallengeVerdict, ConnectCodeError,
    ConnectCodePlaintext, PublishedConnectCode,
};
pub use daemon::{
    DaemonConfig, DaemonError, DaemonStatus, DeviceDaemon, EnrollmentIssuance, ExchangeRequest,
    ExchangeResponse, ExchangeTransport, ExchangeTransportError, LeaseWorkerController,
    TickOutcome, WorkerCapacitySnapshot, WorkerCapacitySource, WorkerLaunchDirectories,
    WorkerLaunchMaterialSource,
};
pub use fencing::{
    FencedCommandKind, FencingGuard, FencingRejection, FencingTicket, FencingVerdict,
};
pub use http::HttpExchangeTransport;
pub use identity::{
    DeviceCredential, DeviceIdentity, DeviceIdentitySeed, IdentityRecord, IssuedEnrollment,
    adopt_enrollment, ensure_device_identity, load_device_identity,
};
pub use path_confinement::{ConfinedPath, ConfinedRoot, ConfinementVerdict, PathConfinementError};
pub use repository::{
    RegistrationOptions, RegistrationRejection, RepositoryBindingSummary, RepositoryRegistration,
    RepositoryRegistryError, RepositoryRemoval, RepositoryRevalidation, list_bindings,
    register_repository, remove_repository, repository_fingerprint, revalidate_repository,
};
pub use repository_git::{
    DETACHED_BRANCH, GitHeadState, GitInspectError, GitInspectOptions, GitInspector, GitScan,
};
pub use store::{
    CLIENT_STORE_SCHEMA_VERSION, ClientInboxCursor, ClientInboxCursorUpdate, ClientOutboxEntry,
    ConnectCodeStateRecord, ConnectionPolicyRecord, DeviceStore, DeviceStoreError,
    DeviceStoreErrorKind, OccupancyMirrorAdvance, OccupancyMirrorRecord, OccupancyMirrorUpdate,
    OccupancyReleaseIntentOutcome, OccupancyReleaseIntentRecord, PathMappingRecord,
    RepositoryLocalStateRecord, ServerProfileRecord, WorkerProcessRecord, availability_wire_name,
    dirty_state_wire_name,
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

// Façade re-export: the connection policy, lock, and connect-code lifecycle
// APIs speak the domain vocabulary, and embedders (the `wwc` CLI) should not
// need a direct `winwincode-client-port` dependency to name it.
pub use winwincode_client_port::domain::{ClientLockState, ConnectCodeState};
