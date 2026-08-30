// SPDX-License-Identifier: Apache-2.0

//! Restricted durable authority for Provider model exchanges.
//!
//! The table stores only request digests, secret-free frozen route authority,
//! adapter request identity, and stable failure/terminal facts. Model request
//! payloads and Provider diagnostics are never accepted by this interface.

use std::fmt;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use winwincode_domain::{ExecutionMessageId, Instant, ModelExchangeId, RequestId, Sha256Digest};

use crate::SqliteStorage;

const PROVIDER_EXCHANGE_SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS internal_provider_exchanges (
    model_exchange_id TEXT PRIMARY KEY NOT NULL,
    request_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    open_digest TEXT NOT NULL,
    provider_id TEXT NOT NULL,
    adapter_request_id TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('opening', 'opened', 'failed', 'terminal')),
    route_authority_fingerprint TEXT,
    route_authority_digest TEXT,
    route_authority_json BLOB,
    open_receipt_digest TEXT,
    open_receipt_json BLOB,
    settlement_context_digest TEXT,
    settlement_context_json BLOB,
    failure_kind TEXT,
    terminal_digest TEXT,
    terminal_receipt_digest TEXT,
    terminal_receipt_json BLOB,
    started_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    CHECK (
        (state = 'opening'
            AND route_authority_fingerprint IS NULL
            AND route_authority_digest IS NULL
            AND route_authority_json IS NULL
            AND open_receipt_digest IS NULL
            AND open_receipt_json IS NULL
            AND settlement_context_digest IS NULL
            AND settlement_context_json IS NULL
            AND failure_kind IS NULL
            AND terminal_digest IS NULL
            AND terminal_receipt_digest IS NULL
            AND terminal_receipt_json IS NULL)
        OR (state = 'opened'
            AND route_authority_fingerprint IS NOT NULL
            AND route_authority_digest IS NOT NULL
            AND route_authority_json IS NOT NULL
            AND open_receipt_digest IS NOT NULL
            AND open_receipt_json IS NOT NULL
            AND settlement_context_digest IS NOT NULL
            AND settlement_context_json IS NOT NULL
            AND failure_kind IS NULL
            AND terminal_digest IS NULL
            AND terminal_receipt_digest IS NULL
            AND terminal_receipt_json IS NULL)
        OR (state = 'failed'
            AND route_authority_fingerprint IS NULL
            AND route_authority_digest IS NULL
            AND route_authority_json IS NULL
            AND open_receipt_digest IS NULL
            AND open_receipt_json IS NULL
            AND settlement_context_digest IS NULL
            AND settlement_context_json IS NULL
            AND failure_kind IS NOT NULL
            AND terminal_digest IS NULL
            AND terminal_receipt_digest IS NULL
            AND terminal_receipt_json IS NULL)
        OR (state = 'terminal'
            AND route_authority_fingerprint IS NOT NULL
            AND route_authority_digest IS NOT NULL
            AND route_authority_json IS NOT NULL
            AND open_receipt_digest IS NOT NULL
            AND open_receipt_json IS NOT NULL
            AND settlement_context_digest IS NOT NULL
            AND settlement_context_json IS NOT NULL
            AND failure_kind IS NULL
            AND terminal_digest IS NOT NULL
            AND terminal_receipt_digest IS NOT NULL
            AND terminal_receipt_json IS NOT NULL)
    )
);
CREATE TABLE IF NOT EXISTS internal_provider_exchange_terminal_progress (
    model_exchange_id TEXT PRIMARY KEY NOT NULL,
    terminal_digest TEXT NOT NULL,
    stage TEXT NOT NULL CHECK (stage IN (
        'prepared', 'cancel_started', 'cancelled', 'release_started', 'released',
        'admission_started', 'admission_settled', 'settlement_started',
        'settlement_settled'
    )),
    admission_receipt_digest TEXT,
    admission_receipt_json BLOB,
    terminal_receipt_digest TEXT,
    terminal_receipt_json BLOB,
    updated_at TEXT NOT NULL,
    CHECK (
        (stage IN ('prepared', 'cancel_started', 'cancelled', 'release_started',
                   'released', 'admission_started')
            AND admission_receipt_digest IS NULL
            AND admission_receipt_json IS NULL
            AND terminal_receipt_digest IS NULL
            AND terminal_receipt_json IS NULL)
        OR (stage IN ('admission_settled', 'settlement_started')
            AND admission_receipt_digest IS NOT NULL
            AND admission_receipt_json IS NOT NULL
            AND terminal_receipt_digest IS NULL
            AND terminal_receipt_json IS NULL)
        OR (stage = 'settlement_settled'
            AND admission_receipt_digest IS NOT NULL
            AND admission_receipt_json IS NOT NULL
            AND terminal_receipt_digest IS NOT NULL
            AND terminal_receipt_json IS NOT NULL)
    ),
    FOREIGN KEY(model_exchange_id) REFERENCES internal_provider_exchanges(model_exchange_id)
);
CREATE TABLE IF NOT EXISTS internal_model_request_pool_authority (
    singleton INTEGER PRIMARY KEY NOT NULL CHECK (singleton = 1),
    state_digest TEXT NOT NULL,
    state_json BLOB NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS internal_provider_exchange_final_acks (
    model_exchange_id TEXT PRIMARY KEY NOT NULL,
    ack_digest TEXT NOT NULL,
    ack_sequence INTEGER NOT NULL CHECK (ack_sequence >= 0),
    receipt_digest TEXT NOT NULL,
    receipt_json BLOB NOT NULL,
    acked_at TEXT NOT NULL,
    FOREIGN KEY(model_exchange_id) REFERENCES internal_provider_exchanges(model_exchange_id)
);
";

const MAX_METADATA_BYTES: usize = 256 * 1024;
const MAX_TOKEN_BYTES: usize = 256;

/// Durable exchange lifecycle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderExchangeState {
    Opening,
    Opened,
    Failed,
    Terminal,
}

/// Durable progress around non-transactional Provider terminal side effects.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ProviderExchangeTerminalStage {
    Prepared,
    CancelStarted,
    Cancelled,
    ReleaseStarted,
    Released,
    AdmissionStarted,
    AdmissionSettled,
    SettlementStarted,
    SettlementSettled,
}

impl ProviderExchangeTerminalStage {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::CancelStarted => "cancel_started",
            Self::Cancelled => "cancelled",
            Self::ReleaseStarted => "release_started",
            Self::Released => "released",
            Self::AdmissionStarted => "admission_started",
            Self::AdmissionSettled => "admission_settled",
            Self::SettlementStarted => "settlement_started",
            Self::SettlementSettled => "settlement_settled",
        }
    }

    fn parse(value: &str) -> Result<Self, ProviderExchangeStoreError> {
        match value {
            "prepared" => Ok(Self::Prepared),
            "cancel_started" => Ok(Self::CancelStarted),
            "cancelled" => Ok(Self::Cancelled),
            "release_started" => Ok(Self::ReleaseStarted),
            "released" => Ok(Self::Released),
            "admission_started" => Ok(Self::AdmissionStarted),
            "admission_settled" => Ok(Self::AdmissionSettled),
            "settlement_started" => Ok(Self::SettlementStarted),
            "settlement_settled" => Ok(Self::SettlementSettled),
            _ => Err(ProviderExchangeStoreError::storage()),
        }
    }
}

/// Secret-free terminal saga checkpoint. A started step is never retried after restart.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderExchangeTerminalProgress {
    pub model_exchange_id: ModelExchangeId,
    pub terminal_digest: Sha256Digest,
    pub stage: ProviderExchangeTerminalStage,
    admission_receipt_json: Option<Vec<u8>>,
    terminal_receipt_json: Option<Vec<u8>>,
    pub updated_at: Instant,
    pub idempotent_replay: bool,
}

/// Complete bounded request-pool authority, including FIFO and route occupancy.
pub struct ModelRequestPoolAuthority {
    state_json: Vec<u8>,
    pub updated_at: Instant,
}

/// Secret-free final Worker acknowledgement tombstone.
pub struct ProviderExchangeFinalAck {
    pub model_exchange_id: ModelExchangeId,
    pub ack_digest: Sha256Digest,
    pub ack_sequence: i64,
    receipt_json: Vec<u8>,
    pub acked_at: Instant,
    pub idempotent_replay: bool,
}

impl ProviderExchangeFinalAck {
    /// Returns the canonical acknowledgement receipt bytes.
    #[must_use]
    pub fn receipt_json(&self) -> &[u8] {
        &self.receipt_json
    }
}

impl fmt::Debug for ProviderExchangeFinalAck {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderExchangeFinalAck")
            .field("model_exchange_id", &self.model_exchange_id)
            .field("ack_digest", &self.ack_digest)
            .field("ack_sequence", &self.ack_sequence)
            .field("receipt_json", &"<redacted>")
            .field("acked_at", &self.acked_at)
            .field("idempotent_replay", &self.idempotent_replay)
            .finish()
    }
}

impl ModelRequestPoolAuthority {
    /// Returns canonical internal pool authority bytes.
    #[must_use]
    pub fn state_json(&self) -> &[u8] {
        &self.state_json
    }
}

impl fmt::Debug for ModelRequestPoolAuthority {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModelRequestPoolAuthority")
            .field("state_json", &"<redacted>")
            .field("updated_at", &self.updated_at)
            .finish()
    }
}

impl ProviderExchangeTerminalProgress {
    /// Returns the secret-free admission receipt after that step commits.
    #[must_use]
    pub fn admission_receipt_json(&self) -> Option<&[u8]> {
        self.admission_receipt_json.as_deref()
    }

    /// Returns the complete terminal receipt after settlement commits.
    #[must_use]
    pub fn terminal_receipt_json(&self) -> Option<&[u8]> {
        self.terminal_receipt_json.as_deref()
    }
}

impl ProviderExchangeState {
    fn parse(value: &str) -> Result<Self, ProviderExchangeStoreError> {
        match value {
            "opening" => Ok(Self::Opening),
            "opened" => Ok(Self::Opened),
            "failed" => Ok(Self::Failed),
            "terminal" => Ok(Self::Terminal),
            _ => Err(ProviderExchangeStoreError::storage()),
        }
    }
}

/// Stable storage failure category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderExchangeStoreErrorCode {
    InvalidInput,
    Conflict,
    InvalidState,
    NotFound,
    Storage,
}

/// Bounded error that never contains retained exchange metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderExchangeStoreError {
    code: ProviderExchangeStoreErrorCode,
    message: &'static str,
}

impl ProviderExchangeStoreError {
    const fn invalid() -> Self {
        Self {
            code: ProviderExchangeStoreErrorCode::InvalidInput,
            message: "Provider exchange input is invalid",
        }
    }

    const fn conflict() -> Self {
        Self {
            code: ProviderExchangeStoreErrorCode::Conflict,
            message: "Provider exchange identity conflicts with durable state",
        }
    }

    const fn state() -> Self {
        Self {
            code: ProviderExchangeStoreErrorCode::InvalidState,
            message: "Provider exchange state rejects this transition",
        }
    }

    const fn not_found() -> Self {
        Self {
            code: ProviderExchangeStoreErrorCode::NotFound,
            message: "Provider exchange was not found",
        }
    }

    const fn storage() -> Self {
        Self {
            code: ProviderExchangeStoreErrorCode::Storage,
            message: "Provider exchange storage operation failed",
        }
    }

    #[must_use]
    pub const fn code(&self) -> ProviderExchangeStoreErrorCode {
        self.code
    }
}

impl fmt::Display for ProviderExchangeStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for ProviderExchangeStoreError {}

/// First durable tombstone written before any Provider or Credential call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderExchangeBegin {
    pub model_exchange_id: ModelExchangeId,
    pub request_id: RequestId,
    pub message_id: ExecutionMessageId,
    pub open_digest: Sha256Digest,
    /// Provider selected by the frozen route before the opening side effect.
    pub provider_id: String,
    /// Precommitted Provider idempotency identity used by open and cleanup.
    pub adapter_request_id: String,
    pub started_at: Instant,
}

/// Secret-free metadata committed after a Provider accepts the exchange.
pub struct ProviderExchangeOpened {
    pub route_authority_fingerprint: Sha256Digest,
    route_authority_json: Vec<u8>,
    open_receipt_json: Vec<u8>,
    settlement_context_json: Vec<u8>,
    pub opened_at: Instant,
}

impl ProviderExchangeOpened {
    /// Seals validated secret-free JSON for durable storage.
    ///
    /// # Errors
    ///
    /// Rejects empty, oversized, or malformed JSON metadata.
    pub fn new(
        route_authority_fingerprint: Sha256Digest,
        route_authority_json: Vec<u8>,
        open_receipt_json: Vec<u8>,
        settlement_context_json: Vec<u8>,
        opened_at: Instant,
    ) -> Result<Self, ProviderExchangeStoreError> {
        validate_digest(&route_authority_fingerprint)?;
        validate_json(&route_authority_json)?;
        validate_json(&open_receipt_json)?;
        validate_json(&settlement_context_json)?;
        validate_instant(&opened_at)?;
        Ok(Self {
            route_authority_fingerprint,
            route_authority_json,
            open_receipt_json,
            settlement_context_json,
            opened_at,
        })
    }
}

impl fmt::Debug for ProviderExchangeOpened {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderExchangeOpened")
            .field(
                "route_authority_fingerprint",
                &self.route_authority_fingerprint,
            )
            .field("route_authority_json", &"<redacted>")
            .field("open_receipt_json", &"<redacted>")
            .field("settlement_context_json", &"<redacted>")
            .field("opened_at", &self.opened_at)
            .finish()
    }
}

/// Stable failed-open terminal fact. No Provider diagnostic is retained.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderExchangeFailure {
    pub failure_kind: String,
    pub failed_at: Instant,
}

/// Secret-free terminal receipt bytes and exact command digest.
pub struct ProviderExchangeTerminal {
    pub terminal_digest: Sha256Digest,
    terminal_receipt_json: Vec<u8>,
    pub settled_at: Instant,
}

impl ProviderExchangeTerminal {
    /// Seals one terminal receipt.
    ///
    /// # Errors
    ///
    /// Rejects a malformed digest, instant, or JSON receipt.
    pub fn new(
        terminal_digest: Sha256Digest,
        terminal_receipt_json: Vec<u8>,
        settled_at: Instant,
    ) -> Result<Self, ProviderExchangeStoreError> {
        validate_digest(&terminal_digest)?;
        validate_json(&terminal_receipt_json)?;
        validate_instant(&settled_at)?;
        Ok(Self {
            terminal_digest,
            terminal_receipt_json,
            settled_at,
        })
    }
}

impl fmt::Debug for ProviderExchangeTerminal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderExchangeTerminal")
            .field("terminal_digest", &self.terminal_digest)
            .field("terminal_receipt_json", &"<redacted>")
            .field("settled_at", &self.settled_at)
            .finish()
    }
}

/// One restricted durable exchange snapshot.
#[derive(Clone, Eq, PartialEq)]
pub struct ProviderExchangeSnapshot {
    pub model_exchange_id: ModelExchangeId,
    pub request_id: RequestId,
    pub message_id: ExecutionMessageId,
    pub open_digest: Sha256Digest,
    pub provider_id: String,
    pub adapter_request_id: String,
    pub state: ProviderExchangeState,
    pub route_authority_fingerprint: Option<Sha256Digest>,
    route_authority_json: Option<Vec<u8>>,
    open_receipt_json: Option<Vec<u8>>,
    settlement_context_json: Option<Vec<u8>>,
    pub failure_kind: Option<String>,
    pub terminal_digest: Option<Sha256Digest>,
    terminal_receipt_json: Option<Vec<u8>>,
    pub started_at: Instant,
    pub updated_at: Instant,
    pub idempotent_replay: bool,
}

impl ProviderExchangeSnapshot {
    /// Returns the restricted frozen-authority JSON.
    #[must_use]
    pub fn route_authority_json(&self) -> Option<&[u8]> {
        self.route_authority_json.as_deref()
    }

    /// Returns the restricted open-receipt JSON.
    #[must_use]
    pub fn open_receipt_json(&self) -> Option<&[u8]> {
        self.open_receipt_json.as_deref()
    }

    /// Returns the restricted retry-settlement context JSON.
    #[must_use]
    pub fn settlement_context_json(&self) -> Option<&[u8]> {
        self.settlement_context_json.as_deref()
    }

    /// Returns the restricted terminal-receipt JSON.
    #[must_use]
    pub fn terminal_receipt_json(&self) -> Option<&[u8]> {
        self.terminal_receipt_json.as_deref()
    }
}

impl fmt::Debug for ProviderExchangeSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderExchangeSnapshot")
            .field("model_exchange_id", &self.model_exchange_id)
            .field("request_id", &self.request_id)
            .field("message_id", &self.message_id)
            .field("open_digest", &self.open_digest)
            .field("provider_id", &self.provider_id)
            .field("adapter_request_id", &self.adapter_request_id)
            .field("state", &self.state)
            .field(
                "route_authority_fingerprint",
                &self.route_authority_fingerprint,
            )
            .field("route_authority_json", &"<redacted>")
            .field("open_receipt_json", &"<redacted>")
            .field("settlement_context_json", &"<redacted>")
            .field("failure_kind", &self.failure_kind)
            .field("terminal_digest", &self.terminal_digest)
            .field("terminal_receipt_json", &"<redacted>")
            .field("started_at", &self.started_at)
            .field("updated_at", &self.updated_at)
            .field("idempotent_replay", &self.idempotent_replay)
            .finish()
    }
}

/// Borrowed restricted Provider exchange store.
pub struct ProviderExchangeStore<'storage> {
    storage: &'storage mut SqliteStorage,
}

impl SqliteStorage {
    /// Opens the restricted Provider exchange authority over this database.
    ///
    /// # Errors
    ///
    /// Returns a stable storage error if the internal schema cannot be prepared.
    pub fn provider_exchange_store(
        &mut self,
    ) -> Result<ProviderExchangeStore<'_>, ProviderExchangeStoreError> {
        ProviderExchangeStore::new(self)
    }
}

impl<'storage> ProviderExchangeStore<'storage> {
    fn new(storage: &'storage mut SqliteStorage) -> Result<Self, ProviderExchangeStoreError> {
        storage
            .connection()
            .map_err(|_| ProviderExchangeStoreError::storage())?
            .execute_batch(PROVIDER_EXCHANGE_SCHEMA)
            .map_err(|_| ProviderExchangeStoreError::storage())?;
        Ok(Self { storage })
    }

    /// Writes the opening tombstone before any Provider-side effect.
    ///
    /// Exact repeats return the existing state. A changed message body,
    /// request identity, or message identity is rejected.
    ///
    /// # Errors
    ///
    /// Rejects malformed input, identity conflicts, or storage failures.
    pub fn begin_open(
        &mut self,
        begin: &ProviderExchangeBegin,
    ) -> Result<ProviderExchangeSnapshot, ProviderExchangeStoreError> {
        validate_begin(begin)?;
        let transaction = self
            .storage
            .connection_mut()
            .map_err(|_| ProviderExchangeStoreError::storage())?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| ProviderExchangeStoreError::storage())?;
        let snapshot = begin_open_in(&transaction, begin)?;
        transaction
            .commit()
            .map_err(|_| ProviderExchangeStoreError::storage())?;
        Ok(snapshot)
    }

    /// Atomically writes the first active pool authority and its opening tombstone.
    ///
    /// This is the only transition from a queued/absent exchange into Provider
    /// opening, so a crash cannot leave an active pool record without a durable
    /// exchange identity.
    ///
    /// # Errors
    ///
    /// Rejects malformed authority bytes, identity conflicts, or storage failures.
    pub fn begin_open_with_pool_authority(
        &mut self,
        begin: &ProviderExchangeBegin,
        pool_authority_json: &[u8],
    ) -> Result<ProviderExchangeSnapshot, ProviderExchangeStoreError> {
        validate_begin(begin)?;
        validate_json(pool_authority_json)?;
        let transaction = self
            .storage
            .connection_mut()
            .map_err(|_| ProviderExchangeStoreError::storage())?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| ProviderExchangeStoreError::storage())?;
        let snapshot = begin_open_in(&transaction, begin)?;
        if !snapshot.idempotent_replay {
            save_pool_authority_in(&transaction, pool_authority_json, &begin.started_at)?;
        }
        transaction
            .commit()
            .map_err(|_| ProviderExchangeStoreError::storage())?;
        Ok(snapshot)
    }

    /// Commits the exact frozen route authority and accepted adapter receipt.
    ///
    /// # Errors
    ///
    /// Rejects missing/open-conflicting state, changed metadata replays, or
    /// storage failures.
    pub fn commit_opened(
        &mut self,
        model_exchange_id: &ModelExchangeId,
        open_digest: &Sha256Digest,
        opened: &ProviderExchangeOpened,
    ) -> Result<ProviderExchangeSnapshot, ProviderExchangeStoreError> {
        validate_exchange_id(model_exchange_id)?;
        validate_digest(open_digest)?;
        let authority_digest = digest_bytes(&opened.route_authority_json);
        let receipt_digest = digest_bytes(&opened.open_receipt_json);
        let context_digest = digest_bytes(&opened.settlement_context_json);
        let transaction = self
            .storage
            .connection_mut()
            .map_err(|_| ProviderExchangeStoreError::storage())?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| ProviderExchangeStoreError::storage())?;
        let existing = load(&transaction, model_exchange_id)?
            .ok_or_else(ProviderExchangeStoreError::not_found)?;
        require_digest(&existing, open_digest)?;
        if existing.state == ProviderExchangeState::Opened {
            require_opened_replay(
                &existing,
                opened,
                &authority_digest,
                &receipt_digest,
                &context_digest,
            )?;
            let mut replay = existing;
            replay.idempotent_replay = true;
            transaction
                .commit()
                .map_err(|_| ProviderExchangeStoreError::storage())?;
            return Ok(replay);
        }
        if existing.state != ProviderExchangeState::Opening {
            return Err(ProviderExchangeStoreError::state());
        }
        transaction
            .execute(
                "UPDATE internal_provider_exchanges
                 SET state = 'opened', route_authority_fingerprint = ?1,
                     route_authority_digest = ?2, route_authority_json = ?3,
                     open_receipt_digest = ?4, open_receipt_json = ?5,
                     settlement_context_digest = ?6, settlement_context_json = ?7,
                     updated_at = ?8
                 WHERE model_exchange_id = ?9 AND state = 'opening'",
                params![
                    opened.route_authority_fingerprint.0,
                    authority_digest.0,
                    opened.route_authority_json,
                    receipt_digest.0,
                    opened.open_receipt_json,
                    context_digest.0,
                    opened.settlement_context_json,
                    opened.opened_at.0,
                    model_exchange_id.0,
                ],
            )
            .map_err(|_| ProviderExchangeStoreError::storage())?;
        let snapshot = load(&transaction, model_exchange_id)?
            .ok_or_else(ProviderExchangeStoreError::storage)?;
        transaction
            .commit()
            .map_err(|_| ProviderExchangeStoreError::storage())?;
        Ok(snapshot)
    }

    /// Commits a stable failed-open tombstone.
    ///
    /// # Errors
    ///
    /// Rejects malformed failure facts, changed replays, invalid state, or
    /// storage failures.
    pub fn commit_failed(
        &mut self,
        model_exchange_id: &ModelExchangeId,
        open_digest: &Sha256Digest,
        failure: &ProviderExchangeFailure,
    ) -> Result<ProviderExchangeSnapshot, ProviderExchangeStoreError> {
        validate_exchange_id(model_exchange_id)?;
        validate_digest(open_digest)?;
        validate_token(&failure.failure_kind)?;
        validate_instant(&failure.failed_at)?;
        let transaction = self
            .storage
            .connection_mut()
            .map_err(|_| ProviderExchangeStoreError::storage())?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| ProviderExchangeStoreError::storage())?;
        let snapshot = commit_failed_in(&transaction, model_exchange_id, open_digest, failure)?;
        transaction
            .commit()
            .map_err(|_| ProviderExchangeStoreError::storage())?;
        Ok(snapshot)
    }

    /// Atomically records a stable failed open and the resulting pool authority.
    ///
    /// # Errors
    ///
    /// Rejects changed failure identity, malformed authority bytes, or storage failures.
    pub fn commit_failed_with_pool_authority(
        &mut self,
        model_exchange_id: &ModelExchangeId,
        open_digest: &Sha256Digest,
        failure: &ProviderExchangeFailure,
        pool_authority_json: &[u8],
    ) -> Result<ProviderExchangeSnapshot, ProviderExchangeStoreError> {
        validate_exchange_id(model_exchange_id)?;
        validate_digest(open_digest)?;
        validate_token(&failure.failure_kind)?;
        validate_instant(&failure.failed_at)?;
        validate_json(pool_authority_json)?;
        let transaction = self
            .storage
            .connection_mut()
            .map_err(|_| ProviderExchangeStoreError::storage())?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| ProviderExchangeStoreError::storage())?;
        let snapshot = commit_failed_in(&transaction, model_exchange_id, open_digest, failure)?;
        save_pool_authority_in(&transaction, pool_authority_json, &failure.failed_at)?;
        transaction
            .commit()
            .map_err(|_| ProviderExchangeStoreError::storage())?;
        Ok(snapshot)
    }

    /// Advances the durable terminal saga before and after each external side effect.
    ///
    /// Exact repeats replay the current checkpoint. A changed command digest or
    /// a backward/out-of-order transition is rejected.
    ///
    /// # Errors
    ///
    /// Rejects missing/non-opened exchanges, changed commands, invalid stage
    /// transitions, or storage failures.
    pub fn record_terminal_progress(
        &mut self,
        model_exchange_id: &ModelExchangeId,
        terminal_digest: &Sha256Digest,
        stage: ProviderExchangeTerminalStage,
        admission_receipt_json: Option<&[u8]>,
        terminal_receipt_json: Option<&[u8]>,
        updated_at: &Instant,
    ) -> Result<ProviderExchangeTerminalProgress, ProviderExchangeStoreError> {
        validate_exchange_id(model_exchange_id)?;
        validate_digest(terminal_digest)?;
        validate_instant(updated_at)?;
        validate_progress_receipts(stage, admission_receipt_json, terminal_receipt_json)?;
        let admission_digest = admission_receipt_json.map(digest_bytes);
        let terminal_receipt_digest = terminal_receipt_json.map(digest_bytes);
        let transaction = self
            .storage
            .connection_mut()
            .map_err(|_| ProviderExchangeStoreError::storage())?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| ProviderExchangeStoreError::storage())?;
        let exchange = load(&transaction, model_exchange_id)?
            .ok_or_else(ProviderExchangeStoreError::not_found)?;
        if exchange.state != ProviderExchangeState::Opened {
            return Err(ProviderExchangeStoreError::state());
        }
        let existing = load_terminal_progress(&transaction, model_exchange_id)?;
        if let Some(mut existing) = existing {
            if existing.terminal_digest != *terminal_digest {
                return Err(ProviderExchangeStoreError::conflict());
            }
            if stage < existing.stage || !valid_terminal_progress_step(existing.stage, stage) {
                return Err(ProviderExchangeStoreError::state());
            }
            if stage == existing.stage {
                if existing.admission_receipt_json.as_deref() != admission_receipt_json
                    || existing.terminal_receipt_json.as_deref() != terminal_receipt_json
                {
                    return Err(ProviderExchangeStoreError::conflict());
                }
                existing.idempotent_replay = true;
                transaction
                    .commit()
                    .map_err(|_| ProviderExchangeStoreError::storage())?;
                return Ok(existing);
            }
            transaction
                .execute(
                    "UPDATE internal_provider_exchange_terminal_progress
                     SET stage = ?1, admission_receipt_digest = ?2,
                         admission_receipt_json = ?3, terminal_receipt_digest = ?4,
                         terminal_receipt_json = ?5, updated_at = ?6
                     WHERE model_exchange_id = ?7",
                    params![
                        stage.as_str(),
                        admission_digest.as_ref().map(|value| &value.0),
                        admission_receipt_json,
                        terminal_receipt_digest.as_ref().map(|value| &value.0),
                        terminal_receipt_json,
                        updated_at.0,
                        model_exchange_id.0,
                    ],
                )
                .map_err(|_| ProviderExchangeStoreError::storage())?;
        } else {
            if stage != ProviderExchangeTerminalStage::Prepared {
                return Err(ProviderExchangeStoreError::state());
            }
            transaction
                .execute(
                    "INSERT INTO internal_provider_exchange_terminal_progress
                        (model_exchange_id, terminal_digest, stage,
                         admission_receipt_digest, admission_receipt_json,
                         terminal_receipt_digest, terminal_receipt_json, updated_at)
                     VALUES (?1, ?2, ?3, NULL, NULL, NULL, NULL, ?4)",
                    params![
                        model_exchange_id.0,
                        terminal_digest.0,
                        stage.as_str(),
                        updated_at.0,
                    ],
                )
                .map_err(|_| ProviderExchangeStoreError::storage())?;
        }
        let progress = load_terminal_progress(&transaction, model_exchange_id)?
            .ok_or_else(ProviderExchangeStoreError::storage)?;
        transaction
            .commit()
            .map_err(|_| ProviderExchangeStoreError::storage())?;
        Ok(progress)
    }

    /// Loads a terminal saga checkpoint without exposing Provider payloads.
    ///
    /// # Errors
    ///
    /// Rejects malformed identifiers or corrupt durable state.
    pub fn load_terminal_progress(
        &self,
        model_exchange_id: &ModelExchangeId,
    ) -> Result<Option<ProviderExchangeTerminalProgress>, ProviderExchangeStoreError> {
        validate_exchange_id(model_exchange_id)?;
        load_terminal_progress(
            self.storage
                .connection()
                .map_err(|_| ProviderExchangeStoreError::storage())?,
            model_exchange_id,
        )
    }

    /// Atomically replaces the complete bounded request-pool authority.
    ///
    /// # Errors
    ///
    /// Rejects malformed JSON/time or unavailable storage.
    pub fn save_pool_authority(
        &mut self,
        state_json: &[u8],
        updated_at: &Instant,
    ) -> Result<ModelRequestPoolAuthority, ProviderExchangeStoreError> {
        validate_json(state_json)?;
        validate_instant(updated_at)?;
        save_pool_authority_in(
            self.storage
                .connection()
                .map_err(|_| ProviderExchangeStoreError::storage())?,
            state_json,
            updated_at,
        )?;
        Ok(ModelRequestPoolAuthority {
            state_json: state_json.to_vec(),
            updated_at: updated_at.clone(),
        })
    }

    /// Loads the complete bounded request-pool authority.
    ///
    /// # Errors
    ///
    /// Returns storage failure for corrupt bytes or digest.
    pub fn load_pool_authority(
        &self,
    ) -> Result<Option<ModelRequestPoolAuthority>, ProviderExchangeStoreError> {
        let raw = self
            .storage
            .connection()
            .map_err(|_| ProviderExchangeStoreError::storage())?
            .query_row(
                "SELECT state_digest, state_json, updated_at
                 FROM internal_model_request_pool_authority WHERE singleton = 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(|_| ProviderExchangeStoreError::storage())?;
        raw.map(|(digest, state_json, updated_at)| {
            validate_json(&state_json)?;
            if digest_bytes(&state_json).0 != digest {
                return Err(ProviderExchangeStoreError::storage());
            }
            let updated_at = Instant(updated_at);
            validate_instant(&updated_at)?;
            Ok(ModelRequestPoolAuthority {
                state_json,
                updated_at,
            })
        })
        .transpose()
    }

    /// Atomically forgets one fully acknowledged terminal buffer, persists the
    /// complete remaining pool authority, and retains a payload-free ack tombstone.
    ///
    /// # Errors
    ///
    /// Rejects non-terminal exchanges, changed acknowledgements, malformed
    /// canonical receipts, or unavailable storage.
    pub fn commit_final_ack_with_pool_authority(
        &mut self,
        model_exchange_id: &ModelExchangeId,
        ack_digest: &Sha256Digest,
        ack_sequence: i64,
        receipt_json: &[u8],
        pool_authority_json: &[u8],
        acked_at: &Instant,
    ) -> Result<ProviderExchangeFinalAck, ProviderExchangeStoreError> {
        validate_exchange_id(model_exchange_id)?;
        validate_digest(ack_digest)?;
        if ack_sequence < 0 {
            return Err(ProviderExchangeStoreError::invalid());
        }
        validate_json(receipt_json)?;
        validate_json(pool_authority_json)?;
        validate_instant(acked_at)?;
        let transaction = self
            .storage
            .connection_mut()
            .map_err(|_| ProviderExchangeStoreError::storage())?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| ProviderExchangeStoreError::storage())?;
        let exchange = load(&transaction, model_exchange_id)?
            .ok_or_else(ProviderExchangeStoreError::not_found)?;
        if exchange.state != ProviderExchangeState::Terminal {
            return Err(ProviderExchangeStoreError::state());
        }
        if let Some(mut replay) = load_final_ack(&transaction, model_exchange_id)? {
            if replay.ack_digest != *ack_digest
                || replay.ack_sequence != ack_sequence
                || replay.receipt_json != receipt_json
            {
                return Err(ProviderExchangeStoreError::conflict());
            }
            replay.idempotent_replay = true;
            transaction
                .commit()
                .map_err(|_| ProviderExchangeStoreError::storage())?;
            return Ok(replay);
        }
        let receipt_digest = digest_bytes(receipt_json);
        transaction
            .execute(
                "INSERT INTO internal_provider_exchange_final_acks
                    (model_exchange_id, ack_digest, ack_sequence, receipt_digest,
                     receipt_json, acked_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    model_exchange_id.0,
                    ack_digest.0,
                    ack_sequence,
                    receipt_digest.0,
                    receipt_json,
                    acked_at.0,
                ],
            )
            .map_err(|_| ProviderExchangeStoreError::storage())?;
        save_pool_authority_in(&transaction, pool_authority_json, acked_at)?;
        let acknowledgement = load_final_ack(&transaction, model_exchange_id)?
            .ok_or_else(ProviderExchangeStoreError::storage)?;
        transaction
            .commit()
            .map_err(|_| ProviderExchangeStoreError::storage())?;
        Ok(acknowledgement)
    }

    /// Loads a final acknowledgement tombstone for exact response replay.
    ///
    /// # Errors
    ///
    /// Rejects malformed identity or corrupt durable bytes.
    pub fn load_final_ack(
        &self,
        model_exchange_id: &ModelExchangeId,
    ) -> Result<Option<ProviderExchangeFinalAck>, ProviderExchangeStoreError> {
        validate_exchange_id(model_exchange_id)?;
        load_final_ack(
            self.storage
                .connection()
                .map_err(|_| ProviderExchangeStoreError::storage())?,
            model_exchange_id,
        )
    }

    /// Commits the exact terminal command and secret-free terminal receipt.
    ///
    /// # Errors
    ///
    /// Rejects changed terminal replays, non-opened exchanges, or storage
    /// failures.
    pub fn commit_terminal(
        &mut self,
        model_exchange_id: &ModelExchangeId,
        terminal: &ProviderExchangeTerminal,
    ) -> Result<ProviderExchangeSnapshot, ProviderExchangeStoreError> {
        validate_exchange_id(model_exchange_id)?;
        let transaction = self
            .storage
            .connection_mut()
            .map_err(|_| ProviderExchangeStoreError::storage())?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| ProviderExchangeStoreError::storage())?;
        let snapshot = commit_terminal_in(&transaction, model_exchange_id, terminal)?;
        transaction
            .commit()
            .map_err(|_| ProviderExchangeStoreError::storage())?;
        Ok(snapshot)
    }

    /// Atomically commits a terminal exchange and the complete resulting pool authority.
    ///
    /// # Errors
    ///
    /// Rejects changed terminal replay, malformed authority bytes, or storage failures.
    pub fn commit_terminal_with_pool_authority(
        &mut self,
        model_exchange_id: &ModelExchangeId,
        terminal: &ProviderExchangeTerminal,
        pool_authority_json: &[u8],
    ) -> Result<ProviderExchangeSnapshot, ProviderExchangeStoreError> {
        validate_exchange_id(model_exchange_id)?;
        validate_json(pool_authority_json)?;
        let transaction = self
            .storage
            .connection_mut()
            .map_err(|_| ProviderExchangeStoreError::storage())?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| ProviderExchangeStoreError::storage())?;
        let snapshot = commit_terminal_in(&transaction, model_exchange_id, terminal)?;
        save_pool_authority_in(&transaction, pool_authority_json, &terminal.settled_at)?;
        transaction
            .commit()
            .map_err(|_| ProviderExchangeStoreError::storage())?;
        Ok(snapshot)
    }

    /// Loads one exchange snapshot.
    ///
    /// # Errors
    ///
    /// Rejects a malformed identifier or corrupt durable record.
    pub fn load(
        &self,
        model_exchange_id: &ModelExchangeId,
    ) -> Result<Option<ProviderExchangeSnapshot>, ProviderExchangeStoreError> {
        validate_exchange_id(model_exchange_id)?;
        load(
            self.storage
                .connection()
                .map_err(|_| ProviderExchangeStoreError::storage())?,
            model_exchange_id,
        )
    }
}

fn begin_open_in(
    connection: &Connection,
    begin: &ProviderExchangeBegin,
) -> Result<ProviderExchangeSnapshot, ProviderExchangeStoreError> {
    if let Some(mut snapshot) = load(connection, &begin.model_exchange_id)? {
        require_open_identity(&snapshot, begin)?;
        snapshot.idempotent_replay = true;
        return Ok(snapshot);
    }
    connection
        .execute(
            "INSERT INTO internal_provider_exchanges
                (model_exchange_id, request_id, message_id, open_digest,
                 provider_id, adapter_request_id, state, started_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'opening', ?7, ?7)",
            params![
                begin.model_exchange_id.0,
                begin.request_id.0,
                begin.message_id.0,
                begin.open_digest.0,
                begin.provider_id,
                begin.adapter_request_id,
                begin.started_at.0,
            ],
        )
        .map_err(|_| ProviderExchangeStoreError::storage())?;
    load(connection, &begin.model_exchange_id)?.ok_or_else(ProviderExchangeStoreError::storage)
}

fn save_pool_authority_in(
    connection: &Connection,
    state_json: &[u8],
    updated_at: &Instant,
) -> Result<(), ProviderExchangeStoreError> {
    let digest = digest_bytes(state_json);
    connection
        .execute(
            "INSERT INTO internal_model_request_pool_authority
                (singleton, state_digest, state_json, updated_at)
             VALUES (1, ?1, ?2, ?3)
             ON CONFLICT(singleton) DO UPDATE SET
                state_digest = excluded.state_digest,
                state_json = excluded.state_json,
                updated_at = excluded.updated_at",
            params![digest.0, state_json, updated_at.0],
        )
        .map_err(|_| ProviderExchangeStoreError::storage())?;
    Ok(())
}

fn commit_failed_in(
    connection: &Connection,
    model_exchange_id: &ModelExchangeId,
    open_digest: &Sha256Digest,
    failure: &ProviderExchangeFailure,
) -> Result<ProviderExchangeSnapshot, ProviderExchangeStoreError> {
    let existing =
        load(connection, model_exchange_id)?.ok_or_else(ProviderExchangeStoreError::not_found)?;
    require_digest(&existing, open_digest)?;
    if existing.state == ProviderExchangeState::Failed {
        if existing.failure_kind.as_deref() != Some(&failure.failure_kind) {
            return Err(ProviderExchangeStoreError::conflict());
        }
        let mut replay = existing;
        replay.idempotent_replay = true;
        return Ok(replay);
    }
    if existing.state != ProviderExchangeState::Opening {
        return Err(ProviderExchangeStoreError::state());
    }
    connection
        .execute(
            "UPDATE internal_provider_exchanges
             SET state = 'failed', failure_kind = ?1, updated_at = ?2
             WHERE model_exchange_id = ?3 AND state = 'opening'",
            params![
                failure.failure_kind,
                failure.failed_at.0,
                model_exchange_id.0
            ],
        )
        .map_err(|_| ProviderExchangeStoreError::storage())?;
    load(connection, model_exchange_id)?.ok_or_else(ProviderExchangeStoreError::storage)
}

fn commit_terminal_in(
    connection: &Connection,
    model_exchange_id: &ModelExchangeId,
    terminal: &ProviderExchangeTerminal,
) -> Result<ProviderExchangeSnapshot, ProviderExchangeStoreError> {
    let receipt_digest = digest_bytes(&terminal.terminal_receipt_json);
    let existing =
        load(connection, model_exchange_id)?.ok_or_else(ProviderExchangeStoreError::not_found)?;
    if existing.state == ProviderExchangeState::Terminal {
        if existing.terminal_digest.as_ref() != Some(&terminal.terminal_digest)
            || existing.terminal_receipt_json.as_deref()
                != Some(terminal.terminal_receipt_json.as_slice())
        {
            return Err(ProviderExchangeStoreError::conflict());
        }
        let mut replay = existing;
        replay.idempotent_replay = true;
        return Ok(replay);
    }
    if existing.state != ProviderExchangeState::Opened {
        return Err(ProviderExchangeStoreError::state());
    }
    connection
        .execute(
            "UPDATE internal_provider_exchanges
             SET state = 'terminal', terminal_digest = ?1,
                 terminal_receipt_digest = ?2, terminal_receipt_json = ?3,
                 updated_at = ?4
             WHERE model_exchange_id = ?5 AND state = 'opened'",
            params![
                terminal.terminal_digest.0,
                receipt_digest.0,
                terminal.terminal_receipt_json,
                terminal.settled_at.0,
                model_exchange_id.0,
            ],
        )
        .map_err(|_| ProviderExchangeStoreError::storage())?;
    load(connection, model_exchange_id)?.ok_or_else(ProviderExchangeStoreError::storage)
}

fn load(
    connection: &Connection,
    model_exchange_id: &ModelExchangeId,
) -> Result<Option<ProviderExchangeSnapshot>, ProviderExchangeStoreError> {
    let snapshot = connection
        .query_row(
            "SELECT request_id, message_id, open_digest, provider_id,
                    adapter_request_id, state,
                    route_authority_fingerprint, route_authority_digest,
                    route_authority_json, open_receipt_digest, open_receipt_json,
                    settlement_context_digest, settlement_context_json, failure_kind,
                    terminal_digest, terminal_receipt_digest, terminal_receipt_json,
                    started_at, updated_at
             FROM internal_provider_exchanges WHERE model_exchange_id = ?1",
            [&model_exchange_id.0],
            |row| {
                Ok(RawSnapshot {
                    request_id: row.get(0)?,
                    message_id: row.get(1)?,
                    open_digest: row.get(2)?,
                    provider_id: row.get(3)?,
                    adapter_request_id: row.get(4)?,
                    state: row.get(5)?,
                    route_authority_fingerprint: row.get(6)?,
                    route_authority_digest: row.get(7)?,
                    route_authority_json: row.get(8)?,
                    open_receipt_digest: row.get(9)?,
                    open_receipt_json: row.get(10)?,
                    settlement_context_digest: row.get(11)?,
                    settlement_context_json: row.get(12)?,
                    failure_kind: row.get(13)?,
                    terminal_digest: row.get(14)?,
                    terminal_receipt_digest: row.get(15)?,
                    terminal_receipt_json: row.get(16)?,
                    started_at: row.get(17)?,
                    updated_at: row.get(18)?,
                })
            },
        )
        .optional()
        .map_err(|_| ProviderExchangeStoreError::storage())?;
    snapshot
        .map(|raw| validate_loaded(model_exchange_id, raw))
        .transpose()
}

fn load_terminal_progress(
    connection: &Connection,
    model_exchange_id: &ModelExchangeId,
) -> Result<Option<ProviderExchangeTerminalProgress>, ProviderExchangeStoreError> {
    let raw = connection
        .query_row(
            "SELECT terminal_digest, stage, admission_receipt_digest,
                    admission_receipt_json, terminal_receipt_digest,
                    terminal_receipt_json, updated_at
             FROM internal_provider_exchange_terminal_progress
             WHERE model_exchange_id = ?1",
            [&model_exchange_id.0],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<Vec<u8>>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<Vec<u8>>>(5)?,
                    row.get::<_, String>(6)?,
                ))
            },
        )
        .optional()
        .map_err(|_| ProviderExchangeStoreError::storage())?;
    raw.map(
        |(
            digest,
            stage,
            admission_receipt_digest,
            admission_receipt_json,
            terminal_receipt_digest,
            terminal_receipt_json,
            updated_at,
        )| {
            let admission_receipt_json =
                verified_json(admission_receipt_json, admission_receipt_digest)?;
            let terminal_receipt_json =
                verified_json(terminal_receipt_json, terminal_receipt_digest)?;
            let progress = ProviderExchangeTerminalProgress {
                model_exchange_id: model_exchange_id.clone(),
                terminal_digest: Sha256Digest(digest),
                stage: ProviderExchangeTerminalStage::parse(&stage)?,
                admission_receipt_json,
                terminal_receipt_json,
                updated_at: Instant(updated_at),
                idempotent_replay: false,
            };
            validate_digest(&progress.terminal_digest)?;
            validate_instant(&progress.updated_at)?;
            validate_progress_receipts(
                progress.stage,
                progress.admission_receipt_json.as_deref(),
                progress.terminal_receipt_json.as_deref(),
            )?;
            Ok(progress)
        },
    )
    .transpose()
}

fn load_final_ack(
    connection: &Connection,
    model_exchange_id: &ModelExchangeId,
) -> Result<Option<ProviderExchangeFinalAck>, ProviderExchangeStoreError> {
    let raw = connection
        .query_row(
            "SELECT ack_digest, ack_sequence, receipt_digest, receipt_json, acked_at
             FROM internal_provider_exchange_final_acks WHERE model_exchange_id = ?1",
            [&model_exchange_id.0],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, String>(4)?,
                ))
            },
        )
        .optional()
        .map_err(|_| ProviderExchangeStoreError::storage())?;
    raw.map(
        |(ack_digest, ack_sequence, receipt_digest, receipt_json, acked_at)| {
            let ack_digest = Sha256Digest(ack_digest);
            validate_digest(&ack_digest)?;
            if ack_sequence < 0 {
                return Err(ProviderExchangeStoreError::storage());
            }
            validate_json(&receipt_json)?;
            if digest_bytes(&receipt_json).0 != receipt_digest {
                return Err(ProviderExchangeStoreError::storage());
            }
            let acked_at = Instant(acked_at);
            validate_instant(&acked_at)?;
            Ok(ProviderExchangeFinalAck {
                model_exchange_id: model_exchange_id.clone(),
                ack_digest,
                ack_sequence,
                receipt_json,
                acked_at,
                idempotent_replay: false,
            })
        },
    )
    .transpose()
}

fn valid_terminal_progress_step(
    current: ProviderExchangeTerminalStage,
    next: ProviderExchangeTerminalStage,
) -> bool {
    current == next
        || matches!(
            (current, next),
            (
                ProviderExchangeTerminalStage::Prepared,
                ProviderExchangeTerminalStage::CancelStarted
                    | ProviderExchangeTerminalStage::ReleaseStarted
            ) | (
                ProviderExchangeTerminalStage::CancelStarted,
                ProviderExchangeTerminalStage::Cancelled
            ) | (
                ProviderExchangeTerminalStage::Cancelled,
                ProviderExchangeTerminalStage::ReleaseStarted
            ) | (
                ProviderExchangeTerminalStage::ReleaseStarted,
                ProviderExchangeTerminalStage::Released
            ) | (
                ProviderExchangeTerminalStage::Released,
                ProviderExchangeTerminalStage::AdmissionStarted
            ) | (
                ProviderExchangeTerminalStage::AdmissionStarted,
                ProviderExchangeTerminalStage::AdmissionSettled
            ) | (
                ProviderExchangeTerminalStage::AdmissionSettled,
                ProviderExchangeTerminalStage::SettlementStarted
            ) | (
                ProviderExchangeTerminalStage::SettlementStarted,
                ProviderExchangeTerminalStage::SettlementSettled
            )
        )
}

fn validate_progress_receipts(
    stage: ProviderExchangeTerminalStage,
    admission_receipt_json: Option<&[u8]>,
    terminal_receipt_json: Option<&[u8]>,
) -> Result<(), ProviderExchangeStoreError> {
    if let Some(bytes) = admission_receipt_json {
        validate_json(bytes)?;
    }
    if let Some(bytes) = terminal_receipt_json {
        validate_json(bytes)?;
    }
    let valid = match stage {
        ProviderExchangeTerminalStage::Prepared
        | ProviderExchangeTerminalStage::CancelStarted
        | ProviderExchangeTerminalStage::Cancelled
        | ProviderExchangeTerminalStage::ReleaseStarted
        | ProviderExchangeTerminalStage::Released
        | ProviderExchangeTerminalStage::AdmissionStarted => {
            admission_receipt_json.is_none() && terminal_receipt_json.is_none()
        }
        ProviderExchangeTerminalStage::AdmissionSettled
        | ProviderExchangeTerminalStage::SettlementStarted => {
            admission_receipt_json.is_some() && terminal_receipt_json.is_none()
        }
        ProviderExchangeTerminalStage::SettlementSettled => {
            admission_receipt_json.is_some() && terminal_receipt_json.is_some()
        }
    };
    if !valid {
        return Err(ProviderExchangeStoreError::invalid());
    }
    Ok(())
}

struct RawSnapshot {
    request_id: String,
    message_id: String,
    open_digest: String,
    provider_id: String,
    adapter_request_id: String,
    state: String,
    route_authority_fingerprint: Option<String>,
    route_authority_digest: Option<String>,
    route_authority_json: Option<Vec<u8>>,
    open_receipt_digest: Option<String>,
    open_receipt_json: Option<Vec<u8>>,
    settlement_context_digest: Option<String>,
    settlement_context_json: Option<Vec<u8>>,
    failure_kind: Option<String>,
    terminal_digest: Option<String>,
    terminal_receipt_digest: Option<String>,
    terminal_receipt_json: Option<Vec<u8>>,
    started_at: String,
    updated_at: String,
}

fn validate_loaded(
    model_exchange_id: &ModelExchangeId,
    raw: RawSnapshot,
) -> Result<ProviderExchangeSnapshot, ProviderExchangeStoreError> {
    let state = ProviderExchangeState::parse(&raw.state)?;
    let route_authority_json = verified_json(raw.route_authority_json, raw.route_authority_digest)?;
    let open_receipt_json = verified_json(raw.open_receipt_json, raw.open_receipt_digest)?;
    let settlement_context_json =
        verified_json(raw.settlement_context_json, raw.settlement_context_digest)?;
    let terminal_receipt_json =
        verified_json(raw.terminal_receipt_json, raw.terminal_receipt_digest)?;
    let snapshot = ProviderExchangeSnapshot {
        model_exchange_id: model_exchange_id.clone(),
        request_id: RequestId(raw.request_id),
        message_id: ExecutionMessageId(raw.message_id),
        open_digest: Sha256Digest(raw.open_digest),
        provider_id: raw.provider_id,
        adapter_request_id: raw.adapter_request_id,
        state,
        route_authority_fingerprint: raw.route_authority_fingerprint.map(Sha256Digest),
        route_authority_json,
        open_receipt_json,
        settlement_context_json,
        failure_kind: raw.failure_kind,
        terminal_digest: raw.terminal_digest.map(Sha256Digest),
        terminal_receipt_json,
        started_at: Instant(raw.started_at),
        updated_at: Instant(raw.updated_at),
        idempotent_replay: false,
    };
    validate_snapshot(&snapshot)?;
    Ok(snapshot)
}

fn verified_json(
    bytes: Option<Vec<u8>>,
    stored_digest: Option<String>,
) -> Result<Option<Vec<u8>>, ProviderExchangeStoreError> {
    match (bytes, stored_digest) {
        (None, None) => Ok(None),
        (Some(bytes), Some(stored_digest)) => {
            validate_json(&bytes)?;
            if digest_bytes(&bytes).0 != stored_digest {
                return Err(ProviderExchangeStoreError::storage());
            }
            Ok(Some(bytes))
        }
        _ => Err(ProviderExchangeStoreError::storage()),
    }
}

fn validate_snapshot(
    snapshot: &ProviderExchangeSnapshot,
) -> Result<(), ProviderExchangeStoreError> {
    validate_exchange_id(&snapshot.model_exchange_id)?;
    validate_token(&snapshot.request_id.0)?;
    validate_token(&snapshot.message_id.0)?;
    validate_digest(&snapshot.open_digest)?;
    validate_token(&snapshot.provider_id)?;
    validate_token(&snapshot.adapter_request_id)?;
    validate_instant(&snapshot.started_at)?;
    validate_instant(&snapshot.updated_at)?;
    if let Some(fingerprint) = &snapshot.route_authority_fingerprint {
        validate_digest(fingerprint)?;
    }
    if let Some(digest) = &snapshot.terminal_digest {
        validate_digest(digest)?;
    }
    if let Some(kind) = &snapshot.failure_kind {
        validate_token(kind)?;
    }
    let opened = snapshot.route_authority_fingerprint.is_some()
        && snapshot.route_authority_json.is_some()
        && snapshot.open_receipt_json.is_some()
        && snapshot.settlement_context_json.is_some();
    let no_opened = snapshot.route_authority_fingerprint.is_none()
        && snapshot.route_authority_json.is_none()
        && snapshot.open_receipt_json.is_none()
        && snapshot.settlement_context_json.is_none();
    let terminal = snapshot.terminal_digest.is_some() && snapshot.terminal_receipt_json.is_some();
    let no_terminal =
        snapshot.terminal_digest.is_none() && snapshot.terminal_receipt_json.is_none();
    let valid = match snapshot.state {
        ProviderExchangeState::Opening => {
            no_opened && snapshot.failure_kind.is_none() && no_terminal
        }
        ProviderExchangeState::Opened => opened && snapshot.failure_kind.is_none() && no_terminal,
        ProviderExchangeState::Failed => {
            no_opened && snapshot.failure_kind.is_some() && no_terminal
        }
        ProviderExchangeState::Terminal => opened && snapshot.failure_kind.is_none() && terminal,
    };
    if !valid {
        return Err(ProviderExchangeStoreError::storage());
    }
    Ok(())
}

fn require_open_identity(
    snapshot: &ProviderExchangeSnapshot,
    begin: &ProviderExchangeBegin,
) -> Result<(), ProviderExchangeStoreError> {
    if snapshot.request_id != begin.request_id
        || snapshot.message_id != begin.message_id
        || snapshot.open_digest != begin.open_digest
        || snapshot.provider_id != begin.provider_id
        || snapshot.adapter_request_id != begin.adapter_request_id
    {
        return Err(ProviderExchangeStoreError::conflict());
    }
    Ok(())
}

fn require_digest(
    snapshot: &ProviderExchangeSnapshot,
    open_digest: &Sha256Digest,
) -> Result<(), ProviderExchangeStoreError> {
    if &snapshot.open_digest != open_digest {
        return Err(ProviderExchangeStoreError::conflict());
    }
    Ok(())
}

fn require_opened_replay(
    existing: &ProviderExchangeSnapshot,
    opened: &ProviderExchangeOpened,
    authority_digest: &Sha256Digest,
    receipt_digest: &Sha256Digest,
    context_digest: &Sha256Digest,
) -> Result<(), ProviderExchangeStoreError> {
    if existing.route_authority_fingerprint.as_ref() != Some(&opened.route_authority_fingerprint)
        || existing.route_authority_json.as_deref() != Some(opened.route_authority_json.as_slice())
        || existing.open_receipt_json.as_deref() != Some(opened.open_receipt_json.as_slice())
        || existing.settlement_context_json.as_deref()
            != Some(opened.settlement_context_json.as_slice())
        || digest_bytes(
            existing
                .route_authority_json
                .as_deref()
                .ok_or_else(ProviderExchangeStoreError::storage)?,
        ) != *authority_digest
        || digest_bytes(
            existing
                .open_receipt_json
                .as_deref()
                .ok_or_else(ProviderExchangeStoreError::storage)?,
        ) != *receipt_digest
        || digest_bytes(
            existing
                .settlement_context_json
                .as_deref()
                .ok_or_else(ProviderExchangeStoreError::storage)?,
        ) != *context_digest
    {
        return Err(ProviderExchangeStoreError::conflict());
    }
    Ok(())
}

fn validate_begin(begin: &ProviderExchangeBegin) -> Result<(), ProviderExchangeStoreError> {
    validate_exchange_id(&begin.model_exchange_id)?;
    validate_token(&begin.request_id.0)?;
    validate_token(&begin.message_id.0)?;
    validate_digest(&begin.open_digest)?;
    validate_token(&begin.provider_id)?;
    validate_token(&begin.adapter_request_id)?;
    validate_instant(&begin.started_at)
}

fn validate_exchange_id(value: &ModelExchangeId) -> Result<(), ProviderExchangeStoreError> {
    validate_token(&value.0)
}

fn validate_token(value: &str) -> Result<(), ProviderExchangeStoreError> {
    if value.is_empty()
        || value.len() > MAX_TOKEN_BYTES
        || value.chars().any(char::is_control)
        || value.contains(['/', '\\'])
    {
        return Err(ProviderExchangeStoreError::invalid());
    }
    Ok(())
}

fn validate_instant(value: &Instant) -> Result<(), ProviderExchangeStoreError> {
    validate_token(&value.0)
}

fn validate_digest(value: &Sha256Digest) -> Result<(), ProviderExchangeStoreError> {
    let Some(hex) = value.0.strip_prefix("sha256:") else {
        return Err(ProviderExchangeStoreError::invalid());
    };
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(ProviderExchangeStoreError::invalid());
    }
    Ok(())
}

fn validate_json(bytes: &[u8]) -> Result<(), ProviderExchangeStoreError> {
    if bytes.is_empty() || bytes.len() > MAX_METADATA_BYTES {
        return Err(ProviderExchangeStoreError::invalid());
    }
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| ProviderExchangeStoreError::invalid())?;
    if !value.is_object() {
        return Err(ProviderExchangeStoreError::invalid());
    }
    Ok(())
}

fn digest_bytes(bytes: &[u8]) -> Sha256Digest {
    Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes)))
}
