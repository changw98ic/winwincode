// SPDX-License-Identifier: Apache-2.0

//! Local device-client data for the multi-user shared `WinWinCode` device
//! client (plan section 8).
//!
//! This crate owns the purely local concerns of a device client:
//!
//! - [`store`]: the on-device `SQLite` database holding the eleven local
//!   tables from plan section 8 (`device_identity`, `device_credential`,
//!   `server_profile`, `repository_path_mapping`, `repository_local_state`,
//!   `occupancy_mirror`, `worker_process_registry`, `worker_launch_receipts`,
//!   `candidate_local_refs`, `client_outbox`, `client_inbox_cursor`), plus
//!   the durable client-to-server outbox adapter implementing the canonical
//!   `winwincode-client-port` exchange traits ([`FrameOutbox`] and
//!   [`CompactingOutbox`]).
//! - [`identity`]: first-boot generation and durable persistence of the
//!   device identity and device credential, including the stable
//!   `publicClientId` and the fresh-per-launch `clientInstanceId`.
//! - [`daemon`]: the periodic device-client exchange loop
//!   (`POST /internal/v1/client/exchange`) over an injected transport:
//!   enrollment, hello announcement, heartbeat reporting, acknowledgement
//!   advancement, gap replay, reacquire handling, and exponential-backoff
//!   recovery on a plain `std` thread — no async runtime.
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

pub mod daemon;
pub mod identity;
pub mod store;

pub use daemon::{
    DaemonConfig, DaemonError, DaemonStatus, DeviceDaemon, ExchangeBatchStatus, ExchangeRequest,
    ExchangeResponse, ExchangeTransport, ExchangeTransportError, TickOutcome,
};
pub use identity::{
    DeviceCredential, DeviceIdentity, DeviceIdentitySeed, IdentityRecord, ensure_device_identity,
};
pub use store::{
    CLIENT_STORE_SCHEMA_VERSION, ClientInboxCursor, ClientInboxCursorUpdate, ClientOutboxEntry,
    DeviceStore, DeviceStoreError, DeviceStoreErrorKind, PathMappingRecord, ServerProfileRecord,
};
