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
//! - [`fencing`]: the pure occupancy fencing decision surface
//!   (CLIENT-300.3, plan 12.6) — [`FencingGuard::authorize_command`] over
//!   the four fenced command entry points (worker launch/stop, candidate
//!   apply, repository mutation), with revision-bound tickets that a mirror
//!   advance invalidates.
//! - [`daemon`]: the periodic device-client exchange loop
//!   (`POST /internal/v1/client/exchange`) over an injected transport:
//!   enrollment adoption, hello announcement, heartbeat reporting,
//!   acknowledgement advancement, gap replay, access-challenge answering,
//!   client-lock application, occupancy mirroring (offer → durable mirror →
//!   ack, release intents, force-fence overwrites), and exponential-backoff
//!   recovery on a plain `std` thread — no async runtime.
//! - [`http`]: the dependency-free std TCP HTTP/1.1 exchange transport
//!   implementation of [`daemon::ExchangeTransport`].
//!
//! The local database never leaves the device: absolute paths stored in
//! `repository_path_mapping` (and worker/candidate data directories) are
//! never uploaded to any server.
//!
//! The SQLite storage patterns (open sequence, migration style, transaction
//! discipline, static-SQL-only rule, and test infrastructure) deliberately
//! mirror `crates/winwincode-storage`, the authoritative local-SQLite
//! storage adapter in this repository.
//!
//! [`FrameOutbox`]: winwincode_client_port::exchange::FrameOutbox
//! [`CompactingOutbox`]: winwincode_client_port::exchange::CompactingOutbox

#![allow(clippy::doc_markdown)]

pub mod connect_code;
pub mod daemon;
pub mod fencing;
pub mod http;
pub mod identity;
pub mod repository;
pub mod store;

pub use connect_code::{
    CONNECT_CODE_DIGITS, CONNECT_CODE_TTL, ChallengeVerdict, ConnectCodeError,
    ConnectCodePlaintext, PublishedConnectCode,
};
pub use daemon::{
    DaemonConfig, DaemonError, DaemonStatus, DeviceDaemon, EnrollmentIssuance, ExchangeRequest,
    ExchangeResponse, ExchangeTransport, ExchangeTransportError, TickOutcome,
};
pub use fencing::{
    FencedCommandKind, FencingGuard, FencingRejection, FencingTicket, FencingVerdict,
};
pub use http::HttpExchangeTransport;
pub use identity::{
    DeviceCredential, DeviceIdentity, DeviceIdentitySeed, IdentityRecord, IssuedEnrollment,
    adopt_enrollment, ensure_device_identity, load_device_identity,
};
pub use repository::{
    RegistrationOptions, RegistrationRejection, RepositoryBindingSummary, RepositoryRegistration,
    RepositoryRegistryError, RepositoryRemoval, RepositoryRevalidation, list_bindings,
    register_repository, remove_repository, repository_fingerprint, revalidate_repository,
};
pub use store::{
    CLIENT_STORE_SCHEMA_VERSION, ClientInboxCursor, ClientInboxCursorUpdate, ClientOutboxEntry,
    ConnectCodeStateRecord, ConnectionPolicyRecord, DeviceStore, DeviceStoreError,
    DeviceStoreErrorKind, OccupancyMirrorAdvance, OccupancyMirrorRecord, OccupancyMirrorUpdate,
    OccupancyReleaseIntentOutcome, OccupancyReleaseIntentRecord, PathMappingRecord,
    RepositoryLocalStateRecord, ServerProfileRecord, availability_wire_name, dirty_state_wire_name,
};

// Façade re-export: the connection policy, lock, and connect-code lifecycle
// APIs speak the domain vocabulary, and embedders (the `wwc` CLI) should not
// need a direct `winwincode-client-port` dependency to name it.
pub use winwincode_client_port::domain::{ClientLockState, ConnectCodeState};
