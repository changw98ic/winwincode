// SPDX-License-Identifier: Apache-2.0

//! Local device-client data for the multi-user shared `WinWinCode` device
//! client (plan section 8).
//!
//! This crate owns the two purely local concerns of a device client:
//!
//! - [`store`]: the on-device `SQLite` database holding the eleven local
//!   tables from plan section 8 (`device_identity`, `device_credential`,
//!   `server_profile`, `repository_path_mapping`, `repository_local_state`,
//!   `occupancy_mirror`, `worker_process_registry`, `worker_launch_receipts`,
//!   `candidate_local_refs`, `client_outbox`, `client_inbox_cursor`).
//! - [`identity`]: first-boot generation and durable persistence of the
//!   device identity and device credential, including the stable
//!   `publicClientId` and the fresh-per-launch `clientInstanceId`.
//!
//! The local database never leaves the device: absolute paths stored in
//! `repository_path_mapping` (and worker/candidate data directories) are
//! never uploaded to any server.
//!
//! The SQLite storage patterns (open sequence, migration style, transaction
//! discipline, static-SQL-only rule, and test infrastructure) deliberately
//! mirror `crates/winwincode-storage`, the authoritative local-SQLite
//! storage adapter in this repository.

#![allow(clippy::doc_markdown)]

pub mod identity;
pub mod store;

pub use identity::{
    DeviceCredential, DeviceIdentity, DeviceIdentitySeed, IdentityRecord, ensure_device_identity,
};
pub use store::{
    CLIENT_STORE_SCHEMA_VERSION, ClientInboxCursor, ClientInboxCursorUpdate, ClientOutboxEntry,
    DeviceStore, DeviceStoreError, DeviceStoreErrorKind, PathMappingRecord,
};

/// Local outbox queue seam used to drain `client_outbox` rows toward the
/// server exchange endpoint (plan section 9.2).
///
/// COMPATIBILITY NOTE (awaiting alignment): the `winwincode-client-port`
/// lane is implementing the canonical outbox trait in parallel and it is
/// not yet on `main`. This trait is a local, structurally compatible
/// stand-in so this lane's storage and tests stay runnable. When the
/// canonical trait lands, this trait must be replaced by (or re-exported
/// from) the `winwincode-client-port` definition and this module's
/// implementation retargeted, without changing the durable `client_outbox`
/// schema.
pub trait StoreOutbox {
    /// Appends one pending client-to-server envelope and returns its durable
    /// outbox sequence.
    ///
    /// # Errors
    ///
    /// Returns [`DeviceStoreErrorKind::Conflict`] when `message_id` already
    /// exists, and [`DeviceStoreErrorKind::Closed`] after the store closed.
    fn append_outbox_envelope(
        &mut self,
        envelope: &winwincode_client_port::messages::ClientToServerEnvelope,
        kind: &str,
    ) -> Result<u64, DeviceStoreError>;

    /// Loads unpublished envelopes in durable sequence order.
    ///
    /// # Errors
    ///
    /// Returns an adapter-neutral error when the read fails or the store is
    /// closed.
    fn pending_outbox_envelopes(&self) -> Result<Vec<ClientOutboxEntry>, DeviceStoreError>;

    /// Marks one envelope as published by its message id.
    ///
    /// # Errors
    ///
    /// Returns an adapter-neutral error when the message id is unknown, the
    /// write fails, or the store is closed.
    fn mark_outbox_published(&mut self, message_id: &str) -> Result<(), DeviceStoreError>;
}
