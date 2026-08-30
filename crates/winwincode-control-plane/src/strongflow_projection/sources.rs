// SPDX-License-Identifier: Apache-2.0

//! Trusted source ports and their bounded, validated read values.

use std::{error::Error, fmt};

use serde::Serialize;
use sha2::{Digest, Sha256};
use winwincode_api::generated::RepositoryScope;
use winwincode_delivery::{
    domain::{Delivery, FrozenDeliveryCandidate},
    projection::runtime::{RuntimeFoldSnapshot, RuntimeProjection, RuntimeSessionProjection},
};
use winwincode_domain::{
    CodexThreadId, DeliveryId, ExecutionEventId, ExecutionJobId, FencingToken, Instant, LeaseId,
    ProductSessionId, Revision, Sha256Digest, WorkerSessionId,
};
use winwincode_storage::{
    ArtifactStore, GitSourceResolver, ProductStateStorage, ProjectionEventCursor,
    ProjectionReadCut, StorageError, StorageErrorKind,
};

use crate::runtime_event_transaction::{
    decode_runtime_ledger_state, product_session_runtime_stream_id_for_projection,
    runtime_stream_id_for_projection,
};

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// Runtime facts exposed by the trusted source seam.
///
/// A Delivery read contains the Delivery-owned runtime sessions. A standalone
/// `ProductSession` read has no Delivery aggregate and therefore deliberately
/// contains no `SessionBinding` rows. Keeping the aggregate identity optional
/// prevents a `ProductSession` read from being represented as a fabricated
/// Delivery or a generic session binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TrustedRuntimeFoldSnapshot {
    pub delivery_id: Option<DeliveryId>,
    pub product_session_id: Option<ProductSessionId>,
    pub sessions: Vec<RuntimeSessionProjection>,
}

/// Exact runtime identity for a standalone `ProductSession` execution.
///
/// `ProductSession` execution uses the typed `(ProductSession, ExecutionJob)`
/// binding rather than a Delivery `SessionBinding` id. The transport contract
/// still exposes one `sessionBindingId` field, so the mapping layer derives a
/// stable projection key from those two durable identities. No independent
/// binding authority is created here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TrustedProductSessionRuntimeSession {
    pub(crate) product_session_id: ProductSessionId,
    pub(crate) execution_job_id: ExecutionJobId,
    pub(crate) worker_session_id: WorkerSessionId,
    pub(crate) codex_thread_id: CodexThreadId,
    pub(crate) lease_id: LeaseId,
    pub(crate) attempt: u64,
    pub(crate) fencing_token: FencingToken,
    pub(crate) as_of_sequence: u64,
}

impl TrustedRuntimeFoldSnapshot {
    fn from_delivery(snapshot: &RuntimeFoldSnapshot) -> Self {
        Self {
            delivery_id: Some(snapshot.delivery_id.clone()),
            product_session_id: None,
            sessions: snapshot.sessions.clone(),
        }
    }

    fn product_session(product_session_id: ProductSessionId) -> Self {
        Self {
            delivery_id: None,
            product_session_id: Some(product_session_id),
            sessions: Vec::new(),
        }
    }
}

/// Expected accepted-ledger coordinates when replaying a server-issued cut.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeCutExpectation {
    ledger_revision: Revision,
    accepted_sequence: u64,
}

impl RuntimeCutExpectation {
    #[must_use]
    pub const fn ledger_revision(&self) -> &Revision {
        &self.ledger_revision
    }

    #[must_use]
    pub const fn accepted_sequence(&self) -> u64 {
        self.accepted_sequence
    }

    pub(crate) const fn new(ledger_revision: Revision, accepted_sequence: u64) -> Self {
        Self {
            ledger_revision,
            accepted_sequence,
        }
    }
}

/// Exact bounded runtime-ledger request created only by the application service.
#[derive(Debug, Clone, PartialEq)]
pub struct DeliveryRuntimeReadRequest {
    scope: RepositoryScope,
    delivery_id: DeliveryId,
    delivery_revision: u64,
    expected: Option<RuntimeCutExpectation>,
    event_cursor: Option<ProjectionEventCursor>,
    limit: usize,
}

/// Exact bounded accepted-ledger request for one `ProductSession`.
#[derive(Debug, Clone, PartialEq)]
pub struct ProductSessionRuntimeReadRequest {
    scope: RepositoryScope,
    product_session_id: ProductSessionId,
    expected: Option<RuntimeCutExpectation>,
    event_cursor: Option<ProjectionEventCursor>,
    limit: usize,
}

impl ProductSessionRuntimeReadRequest {
    #[must_use]
    pub const fn scope(&self) -> &RepositoryScope {
        &self.scope
    }

    #[must_use]
    pub const fn product_session_id(&self) -> &ProductSessionId {
        &self.product_session_id
    }

    #[must_use]
    pub const fn expected(&self) -> Option<&RuntimeCutExpectation> {
        self.expected.as_ref()
    }

    pub(crate) const fn event_cursor(&self) -> Option<&ProjectionEventCursor> {
        self.event_cursor.as_ref()
    }

    #[must_use]
    pub const fn limit(&self) -> usize {
        self.limit
    }

    pub(crate) const fn new(
        scope: RepositoryScope,
        product_session_id: ProductSessionId,
        expected: Option<RuntimeCutExpectation>,
        limit: usize,
    ) -> Self {
        Self {
            scope,
            product_session_id,
            expected,
            event_cursor: None,
            limit,
        }
    }

    pub(crate) fn with_event_cursor(mut self, event_cursor: ProjectionEventCursor) -> Self {
        self.event_cursor = Some(event_cursor);
        self
    }
}

impl DeliveryRuntimeReadRequest {
    #[must_use]
    pub const fn scope(&self) -> &RepositoryScope {
        &self.scope
    }

    #[must_use]
    pub const fn delivery_id(&self) -> &DeliveryId {
        &self.delivery_id
    }

    #[must_use]
    pub const fn delivery_revision(&self) -> u64 {
        self.delivery_revision
    }

    #[must_use]
    pub const fn expected(&self) -> Option<&RuntimeCutExpectation> {
        self.expected.as_ref()
    }

    pub(crate) const fn event_cursor(&self) -> Option<&ProjectionEventCursor> {
        self.event_cursor.as_ref()
    }

    #[must_use]
    pub const fn limit(&self) -> usize {
        self.limit
    }

    pub(crate) fn new(
        scope: RepositoryScope,
        delivery_id: DeliveryId,
        delivery_revision: u64,
        expected: Option<RuntimeCutExpectation>,
        limit: usize,
    ) -> Self {
        Self {
            scope,
            delivery_id,
            delivery_revision,
            expected,
            event_cursor: None,
            limit,
        }
    }

    pub(crate) fn with_event_cursor(mut self, event_cursor: ProjectionEventCursor) -> Self {
        self.event_cursor = Some(event_cursor);
        self
    }
}

/// Adapter-neutral failure from a trusted source owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustedProjectionReadError {
    Unavailable,
    TemporarilyUnavailable,
    /// The adapter once issued the requested exact cut, but its durable
    /// retention window no longer contains it.
    ExactCutNotRetained,
    Stale,
    Invalid,
}

impl fmt::Display for TrustedProjectionReadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unavailable => "trusted facts are unavailable",
            Self::TemporarilyUnavailable => "trusted fact storage is temporarily unavailable",
            Self::ExactCutNotRetained => "trusted fact history no longer retains the exact cut",
            Self::Stale => "trusted facts no longer name the requested cut",
            Self::Invalid => "trusted facts are not canonical",
        })
    }
}

impl Error for TrustedProjectionReadError {}

/// One verified accepted-ledger fold plus its durable cut coordinates.
#[derive(Debug, Clone)]
pub struct TrustedRuntimeProjectionRead {
    scope: RepositoryScope,
    delivery_revision: u64,
    ledger_revision: Revision,
    accepted_sequence: u64,
    rebuilt_at: Instant,
    snapshot: TrustedRuntimeFoldSnapshot,
    product_session_runtime: Option<TrustedProductSessionRuntimeSession>,
    source_seal: Sha256Digest,
    /// Cursor captured by the same durable read as this runtime fact.
    ///
    /// The field is optional for the original in-memory test adapters. A
    /// production adapter must populate it through [`Self::with_event_cursor`]
    /// so the application cannot pair a source read with a cursor obtained by a
    /// later independent read.
    event_cursor: Option<ProjectionEventCursor>,
}

impl TrustedRuntimeProjectionRead {
    /// Validates one projection already rebuilt by the accepted-ledger owner.
    ///
    /// # Errors
    ///
    /// Rejects non-positive revisions, incomplete sequence coordinates, an
    /// oversized fold, unsafe timestamps, or a malformed durable source seal.
    pub fn try_new(
        scope: RepositoryScope,
        delivery_revision: u64,
        ledger_revision: Revision,
        accepted_sequence: u64,
        rebuilt_at: Instant,
        projection: &RuntimeProjection,
        source_seal: Sha256Digest,
    ) -> Result<Self, TrustedProjectionReadError> {
        let snapshot = TrustedRuntimeFoldSnapshot::from_delivery(projection.snapshot());
        Self::try_new_fold(
            scope,
            delivery_revision,
            ledger_revision,
            accepted_sequence,
            rebuilt_at,
            snapshot,
            source_seal,
        )
    }

    fn try_new_fold(
        scope: RepositoryScope,
        delivery_revision: u64,
        ledger_revision: Revision,
        accepted_sequence: u64,
        rebuilt_at: Instant,
        snapshot: TrustedRuntimeFoldSnapshot,
        source_seal: Sha256Digest,
    ) -> Result<Self, TrustedProjectionReadError> {
        let max_session_sequence = snapshot
            .sessions
            .iter()
            .map(|session| session.as_of_sequence)
            .max()
            .unwrap_or(0);
        if delivery_revision == 0
            || delivery_revision > MAX_SAFE_INTEGER
            || !canonical_repository_scope(&scope)
            || snapshot.delivery_id.is_none()
            || !snapshot
                .delivery_id
                .as_ref()
                .is_some_and(|delivery_id| portable(&delivery_id.0, 200))
            || ledger_revision.0 < 0
            || accepted_sequence > MAX_SAFE_INTEGER
            || accepted_sequence < max_session_sequence
            || snapshot.sessions.len() > 256
            || !canonical_instant(&rebuilt_at)
            || !canonical_sha256(&source_seal)
        {
            return Err(TrustedProjectionReadError::Invalid);
        }
        Ok(Self {
            scope,
            delivery_revision,
            ledger_revision,
            accepted_sequence,
            rebuilt_at,
            snapshot,
            product_session_runtime: None,
            source_seal,
            event_cursor: None,
        })
    }

    /// Validates one `ProductSession` runtime ledger read. `ProductSession`
    /// execution is not a Delivery stage and therefore cannot be promoted to
    /// a Delivery `SessionBinding` projection.
    pub(crate) fn try_new_product_session(
        scope: RepositoryScope,
        product_session_id: ProductSessionId,
        ledger_revision: Revision,
        accepted_sequence: u64,
        rebuilt_at: Instant,
        source_seal: Sha256Digest,
        product_session_runtime: Option<TrustedProductSessionRuntimeSession>,
    ) -> Result<Self, TrustedProjectionReadError> {
        if !canonical_repository_scope(&scope)
            || !portable(&product_session_id.0, 200)
            || ledger_revision.0 < 0
            || accepted_sequence > MAX_SAFE_INTEGER
            || !canonical_instant(&rebuilt_at)
            || !canonical_sha256(&source_seal)
            || product_session_runtime.as_ref().is_some_and(|runtime| {
                runtime.product_session_id != product_session_id
                    || runtime.as_of_sequence != accepted_sequence
                    || runtime.attempt == 0
                    || runtime.attempt > MAX_SAFE_INTEGER
                    || !portable(&runtime.execution_job_id.0, 200)
                    || !portable(&runtime.worker_session_id.0, 200)
                    || !portable(&runtime.codex_thread_id.0, 200)
                    || !portable(&runtime.lease_id.0, 200)
                    || !portable(&runtime.fencing_token.0, 200)
            })
        {
            return Err(TrustedProjectionReadError::Invalid);
        }
        Ok(Self {
            scope,
            delivery_revision: 0,
            ledger_revision,
            accepted_sequence,
            rebuilt_at,
            snapshot: TrustedRuntimeFoldSnapshot::product_session(product_session_id),
            product_session_runtime,
            source_seal,
            event_cursor: None,
        })
    }

    /// Associates the exact resource-local event cursor captured with this
    /// runtime fact.
    ///
    /// The cursor is deliberately attached after the trusted runtime owner has
    /// completed its one storage read. It is not a second runtime cursor: it is
    /// the existing public `ProjectionEventCursor` emitted by the same
    /// `StateCommit` that made the accepted ledger state visible.
    #[must_use]
    pub fn with_event_cursor(mut self, event_cursor: ProjectionEventCursor) -> Self {
        self.event_cursor = Some(event_cursor);
        self
    }

    #[must_use]
    pub const fn scope(&self) -> &RepositoryScope {
        &self.scope
    }

    #[must_use]
    pub const fn delivery_revision(&self) -> u64 {
        self.delivery_revision
    }

    #[must_use]
    pub const fn ledger_revision(&self) -> &Revision {
        &self.ledger_revision
    }

    #[must_use]
    pub const fn accepted_sequence(&self) -> u64 {
        self.accepted_sequence
    }

    #[must_use]
    pub const fn rebuilt_at(&self) -> &Instant {
        &self.rebuilt_at
    }

    #[must_use]
    pub fn snapshot(&self) -> &TrustedRuntimeFoldSnapshot {
        &self.snapshot
    }

    /// Returns the resource-local public cursor captured with this read, when
    /// the source owner provides the atomic read-cut seam.
    #[must_use]
    pub const fn event_cursor(&self) -> Option<&ProjectionEventCursor> {
        self.event_cursor.as_ref()
    }

    pub(crate) const fn source_seal(&self) -> &Sha256Digest {
        &self.source_seal
    }

    pub(crate) const fn product_session_runtime(
        &self,
    ) -> Option<&TrustedProductSessionRuntimeSession> {
        self.product_session_runtime.as_ref()
    }
}

/// Trusted append-only runtime-ledger reader.
pub trait TrustedRuntimeProjectionAdapter: Send + Sync {
    /// Reports whether this adapter returns the runtime fact and its public
    /// resource cursor from one storage-owned durable read.
    ///
    /// The default is `false` for the small in-memory adapters used by older
    /// tests. Production adapters must override this and attach the cursor to
    /// every successful read; the application then never performs a separate
    /// cursor-baseline read.
    fn provides_atomic_read_cut(&self) -> bool {
        false
    }

    /// Reads a Delivery runtime fact and its public event cursor through the
    /// storage-owned atomic read-cut seam.
    ///
    /// # Errors
    ///
    /// Returns a stable source failure without substituting an independent
    /// latest cursor read.
    fn read_delivery_with_storage(
        &self,
        storage: &dyn ProductStateStorage,
        request: &DeliveryRuntimeReadRequest,
        delivery: &Delivery,
    ) -> Result<TrustedRuntimeProjectionRead, TrustedProjectionReadError>;

    /// Reads a `ProductSession` runtime fact and its public event cursor through
    /// the storage-owned atomic read-cut seam.
    ///
    /// # Errors
    ///
    /// Returns a stable source failure without substituting an independent
    /// latest cursor read.
    fn read_product_session_with_storage(
        &self,
        storage: &dyn ProductStateStorage,
        request: &ProductSessionRuntimeReadRequest,
    ) -> Result<TrustedRuntimeProjectionRead, TrustedProjectionReadError>;

    /// Reads latest or exact accepted facts for one current aggregate scope.
    ///
    /// # Errors
    ///
    /// Returns a stable source failure without substituting live Worker input.
    fn read_delivery(
        &self,
        request: &DeliveryRuntimeReadRequest,
    ) -> Result<TrustedRuntimeProjectionRead, TrustedProjectionReadError>;

    /// Reads a product-session projection independently of any aggregate cursor.
    ///
    /// # Errors
    ///
    /// The default remains closed until the accepted-ledger adapter implements it.
    fn read_product_session(
        &self,
        _request: &ProductSessionRuntimeReadRequest,
    ) -> Result<TrustedRuntimeProjectionRead, TrustedProjectionReadError> {
        Err(TrustedProjectionReadError::Unavailable)
    }
}

/// One trusted runtime fact and the resource-local event cursor captured by
/// the same durable read.
#[derive(Debug, Clone)]
pub struct TrustedRuntimeProjectionReadCut {
    runtime: TrustedRuntimeProjectionRead,
}

impl TrustedRuntimeProjectionReadCut {
    /// Builds a read cut and attaches its existing storage cursor to the
    /// runtime fact. No cursor is generated or advanced here.
    #[must_use]
    pub fn new(runtime: TrustedRuntimeProjectionRead, event_cursor: ProjectionEventCursor) -> Self {
        Self {
            runtime: runtime.with_event_cursor(event_cursor),
        }
    }

    #[must_use]
    pub const fn runtime(&self) -> &TrustedRuntimeProjectionRead {
        &self.runtime
    }

    /// Returns the resource cursor captured with the runtime fact.
    ///
    /// # Panics
    ///
    /// This invariant cannot be violated because the inner value is private
    /// and [`Self::new`] always attaches the cursor.
    #[must_use]
    pub fn event_cursor(&self) -> &ProjectionEventCursor {
        // `runtime` is private and can only be initialized by `new`, which
        // attaches this cursor before the cut is handed to an adapter.
        self.runtime
            .event_cursor()
            .expect("a read cut always contains its durable event cursor")
    }

    fn into_runtime(self) -> TrustedRuntimeProjectionRead {
        self.runtime
    }
}

/// Storage-owned seam for one atomic runtime fact/cursor read.
///
/// `SqliteStorage` owns the transaction and implements this seam at the
/// composition boundary. The projection module only receives already
/// validated trusted facts and the existing `ProjectionEventCursor`; it never
/// opens a second database connection or invents a parallel cursor table.
pub trait TrustedRuntimeProjectionReadCutReader: Send + Sync {
    /// Reads one Delivery runtime fact and its resource-local cursor at one
    /// durable cut.
    ///
    /// # Errors
    ///
    /// Returns a trusted-source error when the durable cut cannot be read or
    /// fails validation.
    fn read_delivery_cut(
        &self,
        storage: &dyn ProductStateStorage,
        request: &DeliveryRuntimeReadRequest,
        delivery: &Delivery,
    ) -> Result<TrustedRuntimeProjectionReadCut, TrustedProjectionReadError>;

    /// Reads one `ProductSession` runtime fact and its resource-local cursor at
    /// one durable cut.
    ///
    /// # Errors
    ///
    /// Returns a trusted-source error when the durable cut cannot be read or
    /// fails validation.
    fn read_product_session_cut(
        &self,
        storage: &dyn ProductStateStorage,
        request: &ProductSessionRuntimeReadRequest,
    ) -> Result<TrustedRuntimeProjectionReadCut, TrustedProjectionReadError>;
}

/// Production adapter for a storage-owned `SQLite` read-cut implementation.
///
/// The reader is intentionally injected at the deep storage seam: the
/// adapter cannot fall back to a second connection or independently query a
/// latest cursor. This keeps runtime state and its public event baseline
/// inseparable while allowing the storage crate to retain its private `SQLite`
/// transaction handle.
pub struct SqliteTrustedRuntimeProjectionAdapter {
    reader: Box<dyn TrustedRuntimeProjectionReadCutReader>,
}

impl SqliteTrustedRuntimeProjectionAdapter {
    /// Creates the adapter over the storage-owned `SQLite` read-cut seam.
    #[must_use]
    pub fn new(reader: Box<dyn TrustedRuntimeProjectionReadCutReader>) -> Self {
        Self { reader }
    }

    /// Creates the production adapter backed by the `SqliteStorage` read-cut
    /// implementation supplied by the owning `ControlPlane`.
    #[must_use]
    pub fn from_sqlite_storage() -> Self {
        Self::new(Box::new(SqliteStorageRuntimeProjectionReadCutReader))
    }
}

impl TrustedRuntimeProjectionAdapter for SqliteTrustedRuntimeProjectionAdapter {
    fn provides_atomic_read_cut(&self) -> bool {
        true
    }

    fn read_delivery(
        &self,
        request: &DeliveryRuntimeReadRequest,
    ) -> Result<TrustedRuntimeProjectionRead, TrustedProjectionReadError> {
        let _ = request;
        Err(TrustedProjectionReadError::Unavailable)
    }

    fn read_product_session(
        &self,
        request: &ProductSessionRuntimeReadRequest,
    ) -> Result<TrustedRuntimeProjectionRead, TrustedProjectionReadError> {
        let _ = request;
        Err(TrustedProjectionReadError::Unavailable)
    }

    fn read_delivery_with_storage(
        &self,
        storage: &dyn ProductStateStorage,
        request: &DeliveryRuntimeReadRequest,
        delivery: &Delivery,
    ) -> Result<TrustedRuntimeProjectionRead, TrustedProjectionReadError> {
        Ok(self
            .reader
            .read_delivery_cut(storage, request, delivery)?
            .into_runtime())
    }

    fn read_product_session_with_storage(
        &self,
        storage: &dyn ProductStateStorage,
        request: &ProductSessionRuntimeReadRequest,
    ) -> Result<TrustedRuntimeProjectionRead, TrustedProjectionReadError> {
        Ok(self
            .reader
            .read_product_session_cut(storage, request)?
            .into_runtime())
    }
}

/// Production runtime read-cut reader composed with `SqliteStorage`.
///
/// The reader receives the owning storage adapter for each read instead of
/// opening a second cursor source. `ProductStateStorage::load_projection_read_cut`
/// owns the `SQLite` transaction and returns the runtime state streams together
/// with the existing resource-local public event cursor.
#[derive(Debug, Default, Clone, Copy)]
pub struct SqliteStorageRuntimeProjectionReadCutReader;

impl TrustedRuntimeProjectionReadCutReader for SqliteStorageRuntimeProjectionReadCutReader {
    fn read_delivery_cut(
        &self,
        storage: &dyn ProductStateStorage,
        request: &DeliveryRuntimeReadRequest,
        delivery: &Delivery,
    ) -> Result<TrustedRuntimeProjectionReadCut, TrustedProjectionReadError> {
        if request.scope().repository_id.0.is_empty()
            || request.delivery_id() != delivery.id()
            || request.delivery_revision() != delivery.revision()
        {
            return Err(TrustedProjectionReadError::Stale);
        }
        let scope_key = crate::repository_scope_key(request.scope())
            .map_err(|_| TrustedProjectionReadError::Invalid)?;
        let bindings = delivery
            .snapshot()
            .session_bindings
            .iter()
            .filter(|binding| binding.worker_session_id.is_some())
            .collect::<Vec<_>>();
        if bindings.iter().any(|binding| {
            binding.codex_thread_id.is_none()
                || binding.worker_id.is_none()
                || binding.worker_instance_id.is_none()
                || binding.lease_id.is_none()
                || binding.fencing_token.is_none()
        }) {
            return Err(TrustedProjectionReadError::Invalid);
        }
        let state_stream_ids = bindings
            .iter()
            .map(|binding| runtime_stream_id_for_projection(&scope_key, &binding.execution_job_id))
            .collect::<Vec<_>>();
        let event_key =
            super::application::delivery_event_stream_key(request.scope(), request.delivery_id())
                .map_err(|_| TrustedProjectionReadError::Invalid)?;
        let cut = storage
            .load_projection_read_cut(&state_stream_ids, &event_key, request.event_cursor())
            .map_err(|error| map_storage_read_error(&error))?;
        let runtime = delivery_runtime_read(request, delivery, &scope_key, &cut, &bindings)?;
        if runtime.accepted_sequence() > 0 && cut.projection_event_cursor().sequence() == 0 {
            return Err(TrustedProjectionReadError::Invalid);
        }
        Ok(TrustedRuntimeProjectionReadCut::new(
            runtime,
            cut.projection_event_cursor().clone(),
        ))
    }

    fn read_product_session_cut(
        &self,
        storage: &dyn ProductStateStorage,
        request: &ProductSessionRuntimeReadRequest,
    ) -> Result<TrustedRuntimeProjectionReadCut, TrustedProjectionReadError> {
        let state_stream_id =
            product_session_runtime_stream_id_for_projection(request.product_session_id());
        let event_key = super::application::product_session_event_stream_key(
            request.scope(),
            request.product_session_id(),
        )
        .map_err(|_| TrustedProjectionReadError::Invalid)?;
        let cut = storage
            .load_projection_read_cut(
                std::slice::from_ref(&state_stream_id),
                &event_key,
                request.event_cursor(),
            )
            .map_err(|error| map_storage_read_error(&error))?;
        if cut.projection_event_cursor().sequence() == 0 {
            return Err(TrustedProjectionReadError::Invalid);
        }
        let Some(state) = cut.states().first() else {
            let ledger_revision = Revision(0);
            let accepted_sequence = 0;
            if request.expected().is_some_and(|expected| {
                expected.ledger_revision() != &ledger_revision
                    || expected.accepted_sequence() != accepted_sequence
            }) {
                return Err(TrustedProjectionReadError::Stale);
            }
            let read = TrustedRuntimeProjectionRead::try_new_product_session(
                request.scope().clone(),
                request.product_session_id().clone(),
                ledger_revision,
                accepted_sequence,
                Instant("1970-01-01T00:00:00.000Z".to_owned()),
                runtime_source_seal(&cut),
                None,
            )?;
            return Ok(TrustedRuntimeProjectionReadCut::new(
                read,
                cut.projection_event_cursor().clone(),
            ));
        };
        let ledger = decode_runtime_ledger_state(state, &state_stream_id)
            .map_err(|error| map_storage_read_error(&error))?;
        if ledger.product_session_id != *request.product_session_id()
            || ledger.delivery_id.is_some()
            || ledger.delivery_task_id.is_some()
            || ledger.stage_run_id.is_some()
        {
            return Err(TrustedProjectionReadError::Invalid);
        }
        let rebuilt_at = ledger
            .events
            .last()
            .map(|entry| entry.event.occurred_at.clone())
            .ok_or(TrustedProjectionReadError::Invalid)?;
        let product_session_runtime = TrustedProductSessionRuntimeSession {
            product_session_id: ledger.product_session_id.clone(),
            execution_job_id: ledger.execution_job_id.clone(),
            worker_session_id: ledger.worker_session_id.clone(),
            codex_thread_id: ledger.codex_thread_id.clone(),
            lease_id: ledger.lease_id.clone(),
            attempt: ledger.attempt,
            fencing_token: ledger.fencing_token.clone(),
            as_of_sequence: ledger.highest_sequence,
        };
        let ledger_revision = Revision(
            i64::try_from(ledger.highest_sequence)
                .map_err(|_| TrustedProjectionReadError::Invalid)?,
        );
        let accepted_sequence = ledger.highest_sequence;
        let source_seal = runtime_source_seal(&cut);
        let expected_mismatch = request.expected().is_some_and(|expected| {
            expected.ledger_revision() != &ledger_revision
                || expected.accepted_sequence() != accepted_sequence
        });
        let read = TrustedRuntimeProjectionRead::try_new_product_session(
            request.scope().clone(),
            request.product_session_id().clone(),
            ledger_revision,
            accepted_sequence,
            rebuilt_at,
            source_seal,
            Some(product_session_runtime),
        )?;
        if expected_mismatch {
            return Err(TrustedProjectionReadError::Stale);
        }
        Ok(TrustedRuntimeProjectionReadCut::new(
            read,
            cut.projection_event_cursor().clone(),
        ))
    }
}

fn delivery_runtime_read(
    request: &DeliveryRuntimeReadRequest,
    delivery: &Delivery,
    scope_key: &winwincode_storage::ReceiptScopeKey,
    cut: &ProjectionReadCut,
    bindings: &[&winwincode_delivery::domain::SessionBinding],
) -> Result<TrustedRuntimeProjectionRead, TrustedProjectionReadError> {
    if bindings.is_empty() {
        let projection = RuntimeProjection::new(delivery, Vec::new())
            .map_err(|_| TrustedProjectionReadError::Invalid)?;
        return trusted_runtime_read(
            request,
            &projection,
            Revision(0),
            0,
            Instant("1970-01-01T00:00:00.000Z".to_owned()),
            cut,
        );
    }
    let mut sessions = Vec::with_capacity(bindings.len());
    let mut accepted_sequence = 0_u64;
    let mut rebuilt_at = Instant("1970-01-01T00:00:00.000Z".to_owned());
    for binding in bindings {
        let loaded = load_delivery_runtime_state(delivery, binding, scope_key, cut)?;
        let stage_run_settled = delivery
            .snapshot()
            .stage_runs
            .iter()
            .find(|run| run.id == binding.stage_run_id)
            .ok_or(TrustedProjectionReadError::Invalid)?
            .finished_at_millis
            .is_some();
        let settled_last_sequence = if stage_run_settled {
            if loaded.accepted_sequence == 0 {
                return Err(TrustedProjectionReadError::Invalid);
            }
            Some(loaded.accepted_sequence)
        } else {
            None
        };
        let projection = RuntimeProjection::from_persisted_checkpoints(
            delivery,
            &binding.id,
            binding
                .lease_id
                .clone()
                .ok_or(TrustedProjectionReadError::Invalid)?,
            binding
                .fencing_token
                .clone()
                .ok_or(TrustedProjectionReadError::Invalid)?,
            binding
                .worker_id
                .clone()
                .ok_or(TrustedProjectionReadError::Invalid)?,
            binding
                .worker_instance_id
                .clone()
                .ok_or(TrustedProjectionReadError::Invalid)?,
            settled_last_sequence,
            &loaded.events,
        )
        .map_err(|_| TrustedProjectionReadError::Invalid)?;
        let [session] = projection.snapshot().sessions.as_slice() else {
            return Err(TrustedProjectionReadError::Invalid);
        };
        sessions.push(session.clone());
        accepted_sequence = accepted_sequence
            .checked_add(loaded.accepted_sequence)
            .filter(|sequence| *sequence <= MAX_SAFE_INTEGER)
            .ok_or(TrustedProjectionReadError::Invalid)?;
        if loaded.rebuilt_at.0 > rebuilt_at.0 {
            rebuilt_at = loaded.rebuilt_at;
        }
    }
    sessions.sort_by(|left, right| left.session_binding_id.0.cmp(&right.session_binding_id.0));
    let ledger_revision = Revision(
        i64::try_from(accepted_sequence).map_err(|_| TrustedProjectionReadError::Invalid)?,
    );
    trusted_runtime_fold_read(
        request,
        TrustedRuntimeFoldSnapshot {
            delivery_id: Some(delivery.id().clone()),
            product_session_id: None,
            sessions,
        },
        ledger_revision,
        accepted_sequence,
        rebuilt_at,
        cut,
    )
}

struct LoadedDeliveryRuntimeState {
    events: Vec<(u64, ExecutionEventId)>,
    accepted_sequence: u64,
    rebuilt_at: Instant,
}

fn load_delivery_runtime_state(
    delivery: &Delivery,
    binding: &winwincode_delivery::domain::SessionBinding,
    scope_key: &winwincode_storage::ReceiptScopeKey,
    cut: &ProjectionReadCut,
) -> Result<LoadedDeliveryRuntimeState, TrustedProjectionReadError> {
    let stream_id = runtime_stream_id_for_projection(scope_key, &binding.execution_job_id);
    let ledger = cut
        .states()
        .iter()
        .find(|state| state.stream_id == stream_id)
        .map(|state| decode_runtime_ledger_state(state, &stream_id))
        .transpose()
        .map_err(|error| map_storage_read_error(&error))?;
    let Some(ledger) = ledger else {
        return Ok(LoadedDeliveryRuntimeState {
            events: Vec::new(),
            accepted_sequence: 0,
            rebuilt_at: Instant("1970-01-01T00:00:00.000Z".to_owned()),
        });
    };
    let (
        Some(worker_session_id),
        Some(codex_thread_id),
        Some(lease_id),
        Some(fencing_token),
        Some(worker_id),
        Some(worker_instance_id),
    ) = (
        binding.worker_session_id.as_ref(),
        binding.codex_thread_id.as_ref(),
        binding.lease_id.as_ref(),
        binding.fencing_token.as_ref(),
        binding.worker_id.as_ref(),
        binding.worker_instance_id.as_ref(),
    )
    else {
        return Err(TrustedProjectionReadError::Invalid);
    };
    if ledger.delivery_id.as_ref() != Some(delivery.id())
        || ledger.delivery_task_id.as_ref() != binding.delivery_task_id.as_ref()
        || ledger.stage_run_id.as_ref() != Some(&binding.stage_run_id)
        || ledger.product_session_id != binding.product_session_id
        || ledger.execution_job_id != binding.execution_job_id
        || &ledger.worker_session_id != worker_session_id
        || &ledger.codex_thread_id != codex_thread_id
        || &ledger.lease_id != lease_id
        || ledger.attempt != binding.attempt
        || &ledger.fencing_token != fencing_token
        || &ledger.worker_id != worker_id
        || &ledger.worker_instance_id != worker_instance_id
    {
        return Err(TrustedProjectionReadError::Invalid);
    }
    let rebuilt_at = ledger
        .events
        .last()
        .map(|entry| entry.event.occurred_at.clone())
        .ok_or(TrustedProjectionReadError::Invalid)?;
    let events = ledger
        .events
        .iter()
        .map(|entry| {
            Ok::<_, TrustedProjectionReadError>((
                u64::try_from(entry.event.sequence.0)
                    .map_err(|_| TrustedProjectionReadError::Invalid)?,
                entry.event.event_id.clone(),
            ))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(LoadedDeliveryRuntimeState {
        events,
        accepted_sequence: ledger.highest_sequence,
        rebuilt_at,
    })
}

fn trusted_runtime_read(
    request: &DeliveryRuntimeReadRequest,
    projection: &RuntimeProjection,
    ledger_revision: Revision,
    accepted_sequence: u64,
    rebuilt_at: Instant,
    cut: &ProjectionReadCut,
) -> Result<TrustedRuntimeProjectionRead, TrustedProjectionReadError> {
    trusted_runtime_fold_read(
        request,
        TrustedRuntimeFoldSnapshot::from_delivery(projection.snapshot()),
        ledger_revision,
        accepted_sequence,
        rebuilt_at,
        cut,
    )
}

fn trusted_runtime_fold_read(
    request: &DeliveryRuntimeReadRequest,
    snapshot: TrustedRuntimeFoldSnapshot,
    ledger_revision: Revision,
    accepted_sequence: u64,
    rebuilt_at: Instant,
    cut: &ProjectionReadCut,
) -> Result<TrustedRuntimeProjectionRead, TrustedProjectionReadError> {
    let source_seal = runtime_source_seal(cut);
    let read = TrustedRuntimeProjectionRead::try_new_fold(
        request.scope().clone(),
        request.delivery_revision(),
        ledger_revision,
        accepted_sequence,
        rebuilt_at,
        snapshot,
        source_seal,
    )?;
    if request.expected().is_some_and(|expected| {
        expected.ledger_revision() != read.ledger_revision()
            || expected.accepted_sequence() != accepted_sequence
    }) {
        return Err(TrustedProjectionReadError::Stale);
    }
    Ok(read.with_event_cursor(cut.projection_event_cursor().clone()))
}

fn runtime_source_seal(cut: &ProjectionReadCut) -> Sha256Digest {
    let mut digest = Sha256::new();
    digest.update(b"winwincode.runtime-projection-source.v1\0");
    for state in cut.states() {
        digest.update((state.stream_id.len() as u64).to_be_bytes());
        digest.update(state.stream_id.as_bytes());
        digest.update(state.revision.to_be_bytes());
        digest.update((state.payload.len() as u64).to_be_bytes());
        digest.update(&state.payload);
    }
    digest.update(cut.projection_event_cursor().sequence().to_be_bytes());
    if let Some(event_id) = cut.projection_event_cursor().event_id() {
        digest.update((event_id.0.len() as u64).to_be_bytes());
        digest.update(event_id.0.as_bytes());
    }
    Sha256Digest(format!("sha256:{:x}", digest.finalize()))
}

fn map_storage_read_error(error: &StorageError) -> TrustedProjectionReadError {
    match error.kind() {
        StorageErrorKind::EventCursorExpired => TrustedProjectionReadError::ExactCutNotRetained,
        StorageErrorKind::InvalidInput | StorageErrorKind::RevisionConflict => {
            TrustedProjectionReadError::Invalid
        }
        StorageErrorKind::Adapter => TrustedProjectionReadError::Unavailable,
        _ => TrustedProjectionReadError::TemporarilyUnavailable,
    }
}

pub use winwincode_publication::{
    PublicationFactBinding, PublicationResourceFact, PublicationResourceKind, PublicationResultFact,
};

/// Candidate and publication facts read from one durable publication cut.
#[derive(Debug, Clone, PartialEq)]
pub struct TrustedPublicationProjectionRead {
    scope: RepositoryScope,
    delivery_id: DeliveryId,
    delivery_revision: u64,
    publication_revision: Revision,
    candidate: Option<FrozenDeliveryCandidate>,
    result: Option<PublicationResultFact>,
    source_seal: Sha256Digest,
}

impl TrustedPublicationProjectionRead {
    /// Builds one sealed publication-ledger read.
    ///
    /// # Errors
    ///
    /// Rejects unsafe revisions, source seals, or a mismatched result revision.
    pub fn try_new(
        scope: RepositoryScope,
        delivery_id: DeliveryId,
        delivery_revision: u64,
        publication_revision: Revision,
        candidate: Option<FrozenDeliveryCandidate>,
        result: Option<PublicationResultFact>,
        source_seal: Sha256Digest,
    ) -> Result<Self, TrustedProjectionReadError> {
        if delivery_revision == 0
            || delivery_revision > MAX_SAFE_INTEGER
            || !canonical_repository_scope(&scope)
            || !portable(&delivery_id.0, 200)
            || publication_revision.0 < 0
            || !canonical_sha256(&source_seal)
            || candidate
                .as_ref()
                .is_some_and(|candidate| candidate.delivery_id() != &delivery_id)
            || result.as_ref().is_some_and(|result| {
                result.revision() != &publication_revision
                    || result.binding().delivery_id() != &delivery_id
                    || result.binding().delivery_revision() != delivery_revision
            })
        {
            return Err(TrustedProjectionReadError::Invalid);
        }
        Ok(Self {
            scope,
            delivery_id,
            delivery_revision,
            publication_revision,
            candidate,
            result,
            source_seal,
        })
    }

    #[must_use]
    pub const fn scope(&self) -> &RepositoryScope {
        &self.scope
    }

    #[must_use]
    pub const fn delivery_id(&self) -> &DeliveryId {
        &self.delivery_id
    }

    #[must_use]
    pub const fn delivery_revision(&self) -> u64 {
        self.delivery_revision
    }

    #[must_use]
    pub const fn publication_revision(&self) -> &Revision {
        &self.publication_revision
    }

    #[must_use]
    pub const fn candidate(&self) -> Option<&FrozenDeliveryCandidate> {
        self.candidate.as_ref()
    }

    #[must_use]
    pub const fn result(&self) -> Option<&PublicationResultFact> {
        self.result.as_ref()
    }

    pub(crate) const fn source_seal(&self) -> &Sha256Digest {
        &self.source_seal
    }
}

/// Trusted Git/publication intent and result reader.
pub trait TrustedPublicationProjectionAdapter: Send + Sync {
    /// Reads publication facts through the Control Plane's canonical durable
    /// storage, Artifact store, and Git source resolver.
    ///
    /// Production adapters override this method. The default preserves the
    /// small in-memory adapter seam while ensuring that application code has
    /// one call path for local and injected sources.
    ///
    /// # Errors
    ///
    /// Returns a stable source failure when the exact durable facts cannot be
    /// reconstructed or have changed.
    #[allow(
        clippy::too_many_arguments,
        reason = "the trusted read binds every independent durable authority explicitly"
    )]
    fn read_current_with_storage(
        &self,
        _storage: &dyn ProductStateStorage,
        _artifacts: Option<&ArtifactStore>,
        _source_resolver: Option<&dyn GitSourceResolver>,
        _delivery: &Delivery,
        scope: &RepositoryScope,
        delivery_id: &DeliveryId,
        delivery_revision: u64,
        expected_publication_revision: Option<&Revision>,
    ) -> Result<TrustedPublicationProjectionRead, TrustedProjectionReadError> {
        self.read_current(
            scope,
            delivery_id,
            delivery_revision,
            expected_publication_revision,
        )
    }

    /// Reads latest or exact publication facts for one aggregate revision.
    ///
    /// # Errors
    ///
    /// Returns a stable source failure without treating a missing adapter as an
    /// empty successful publication set.
    fn read_current(
        &self,
        scope: &RepositoryScope,
        delivery_id: &DeliveryId,
        delivery_revision: u64,
        expected_publication_revision: Option<&Revision>,
    ) -> Result<TrustedPublicationProjectionRead, TrustedProjectionReadError>;
}

/// Runtime and publication adapters installed as one immutable authority set.
pub struct StrongFlowProjectionSources {
    pub(crate) runtime: Box<dyn TrustedRuntimeProjectionAdapter>,
    pub(crate) publication: Box<dyn TrustedPublicationProjectionAdapter>,
}

impl StrongFlowProjectionSources {
    #[must_use]
    pub fn new(
        runtime: Box<dyn TrustedRuntimeProjectionAdapter>,
        publication: Box<dyn TrustedPublicationProjectionAdapter>,
    ) -> Self {
        Self {
            runtime,
            publication,
        }
    }
}

fn canonical_sha256(value: &Sha256Digest) -> bool {
    value
        .0
        .strip_prefix("sha256:")
        .is_some_and(lowercase_sha256)
}

fn lowercase_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn canonical_instant(value: &Instant) -> bool {
    let text = value.0.as_str();
    text.len() >= 20
        && text.len() <= 40
        && text.ends_with('Z')
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'-' | b':' | b'.' | b'T' | b'Z'))
}

fn portable(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'.' | b'_' | b':' | b'/' | b'@' | b'-' | b'#')
        })
}

fn canonical_repository_scope(scope: &RepositoryScope) -> bool {
    scope.kind == winwincode_api::generated::RepositoryScopeKind::Repository
        && portable_scope_id(&scope.organization_id.0)
        && portable_scope_id(&scope.workspace_id.0)
        && portable_scope_id(&scope.project_id.0)
        && portable_scope_id(&scope.repository_id.0)
}

fn portable_scope_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 200
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'@' | b'-')
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_event_transaction::{RuntimeLedgerEvent, RuntimeLedgerState};
    use winwincode_api::generated::{
        ControlPlaneWebSocketDeliveryGetReloadQuery,
        ControlPlaneWebSocketDeliveryStageRuntimeProjectionInvalidatedEvent,
        ControlPlaneWebSocketDeliveryStageRuntimeProjectionInvalidatedEventScopeKind,
        ControlPlaneWebSocketDeliveryStageRuntimeProjectionInvalidatedEventTypeValue,
        ControlPlaneWebSocketProductSessionRuntimeProjectionInvalidatedEvent,
        ControlPlaneWebSocketProductSessionRuntimeProjectionInvalidatedEventScopeKind,
        ControlPlaneWebSocketProductSessionRuntimeProjectionInvalidatedEventTypeValue,
        ControlPlaneWebSocketRuntimeProjectionGetReloadQuery,
    };
    use winwincode_delivery::{
        domain::{Delivery, SessionBindingId},
        projection::runtime::{
            RuntimeProjection,
            test_support::{
                RuntimeAuthorityFixture, RuntimeFactFixture, accepted_binding, accepted_event,
            },
        },
    };
    use winwincode_domain::{
        ControlPlaneEventId, ExecutionEventId, ExecutionJobId, ExecutionSequence, FencingToken,
        Instant as DomainInstant, LeaseId, RequestId, Revision, SessionIdentity, WorkerId,
        WorkerInstanceId, WorkerSessionId,
    };
    use winwincode_execution_port::generated::{
        ExecutionEventCategory, ExecutionEventRecord, ExecutionLeaseStamp,
    };
    use winwincode_storage::{
        NewOutboxEvent, ProductStateStorage, ProjectionEventStream, ProjectionEventStreamKey,
        PublicEventActor, PublicEventSource, ReceiptIdentity, ReceiptScopeKey, StateCommit,
        receipt_actor_key,
    };

    fn scope() -> RepositoryScope {
        RepositoryScope {
            kind: winwincode_api::generated::RepositoryScopeKind::Repository,
            organization_id: winwincode_domain::OrganizationId("org_fixture".into()),
            workspace_id: winwincode_domain::WorkspaceId("wsp_fixture".into()),
            project_id: winwincode_domain::ProjectId("prj_fixture".into()),
            repository_id: winwincode_domain::RepositoryId("rep_fixture".into()),
        }
    }

    fn canonical_source_scope() -> RepositoryScope {
        RepositoryScope {
            kind: winwincode_api::generated::RepositoryScopeKind::Repository,
            organization_id: winwincode_domain::OrganizationId(
                "org_01J00000000000000000000000".into(),
            ),
            workspace_id: winwincode_domain::WorkspaceId("wsp_01J00000000000000000000000".into()),
            project_id: winwincode_domain::ProjectId("prj_01J00000000000000000000000".into()),
            repository_id: winwincode_domain::RepositoryId("rep_01J00000000000000000000000".into()),
        }
    }

    fn source_runtime_event(event_id: &str) -> (ExecutionEventRecord, Sha256Digest) {
        let event = ExecutionEventRecord {
            category: ExecutionEventCategory::Lifecycle,
            event_id: ExecutionEventId(event_id.into()),
            occurred_at: DomainInstant("2026-08-25T00:00:00Z".into()),
            payload: None,
            sequence: ExecutionSequence(1),
            summary: "runtime accepted".into(),
        };
        let digest = Sha256Digest(format!(
            "sha256:{:x}",
            Sha256::digest(serde_json::to_vec(&event).expect("event JSON")),
        ));
        (event, digest)
    }

    struct RuntimeFixtureCommit<'fixture> {
        request_id: &'fixture str,
        command_digest: char,
        stream_id: String,
        ledger: &'fixture RuntimeLedgerState,
        event_id: &'fixture str,
        stream: ProjectionEventStream,
    }

    fn commit_runtime_fixture(
        storage: &mut winwincode_storage::SqliteStorage,
        scope: &RepositoryScope,
        fixture: RuntimeFixtureCommit<'_>,
    ) {
        let scope_key = crate::repository_scope_key(scope).expect("scope key");
        let actor = PublicEventActor::System {
            id: winwincode_domain::SystemActorId("sys_01J00000000000000000000000".into()),
        };
        let receipt_identity = ReceiptIdentity::new(
            receipt_actor_key(&actor).expect("actor key"),
            scope_key,
            RequestId(fixture.request_id.into()),
        )
        .expect("receipt identity");
        let accepted_sequence = i64::try_from(fixture.ledger.highest_sequence)
            .expect("fixture sequence in public range");
        let payload = if let Some(delivery_id) = &fixture.ledger.delivery_id {
            let stage_run_id = fixture
                .ledger
                .stage_run_id
                .clone()
                .expect("Delivery fixture StageRun");
            serde_json::to_vec(
                &ControlPlaneWebSocketDeliveryStageRuntimeProjectionInvalidatedEvent {
                    delivery_id: delivery_id.clone(),
                    last_projection_sequence: accepted_sequence,
                    product_session_id: fixture.ledger.product_session_id.clone(),
                    projection_revision: Revision(accepted_sequence),
                    reload_queries: (
                        ControlPlaneWebSocketDeliveryGetReloadQuery::DeliveryGet,
                        ControlPlaneWebSocketRuntimeProjectionGetReloadQuery::RuntimeProjectionGet,
                    ),
                    scope_kind:
                        ControlPlaneWebSocketDeliveryStageRuntimeProjectionInvalidatedEventScopeKind::DeliveryStage,
                    session_identity: SessionIdentity {
                        codex_thread_id: fixture.ledger.codex_thread_id.clone(),
                        product_session_id: fixture.ledger.product_session_id.clone(),
                        stage_run_id: Some(stage_run_id.clone()),
                        worker_session_id: fixture.ledger.worker_session_id.clone(),
                    },
                    stage_run_id,
                    type_value:
                        ControlPlaneWebSocketDeliveryStageRuntimeProjectionInvalidatedEventTypeValue::RuntimeProjectionInvalidatedV1,
                },
            )
            .expect("generated Delivery runtime invalidation")
        } else {
            serde_json::to_vec(
                &ControlPlaneWebSocketProductSessionRuntimeProjectionInvalidatedEvent {
                    last_projection_sequence: accepted_sequence,
                    product_session_id: fixture.ledger.product_session_id.clone(),
                    projection_revision: Revision(accepted_sequence),
                    reload_queries: (
                        ControlPlaneWebSocketRuntimeProjectionGetReloadQuery::RuntimeProjectionGet,
                    ),
                    scope_kind:
                        ControlPlaneWebSocketProductSessionRuntimeProjectionInvalidatedEventScopeKind::ProductSession,
                    type_value:
                        ControlPlaneWebSocketProductSessionRuntimeProjectionInvalidatedEventTypeValue::RuntimeProjectionInvalidatedV1,
                },
            )
            .expect("generated ProductSession runtime invalidation")
        };
        storage
            .commit(&StateCommit::new(
                receipt_identity,
                Sha256Digest(format!(
                    "sha256:{}",
                    fixture.command_digest.to_string().repeat(64)
                )),
                fixture.stream_id,
                0,
                serde_json::to_vec(fixture.ledger).expect("ledger JSON"),
                vec![
                    NewOutboxEvent::public_projection(
                        ControlPlaneEventId(fixture.event_id.into()),
                        "runtime-projection.invalidated.v1",
                        payload,
                        fixture.stream,
                        crate::public_repository_scope(scope),
                        DomainInstant("2026-08-25T00:00:00.000Z".into()),
                        PublicEventSource::ControlPlane {
                            actor,
                            component: "strongflow-projection-fixture".into(),
                        },
                    )
                    .expect("public fixture event"),
                ],
            ))
            .expect("runtime state and invalidation");
    }

    fn runtime_projection() -> RuntimeProjection {
        let aggregate = Delivery::decode_json(include_bytes!(
            "../../../winwincode-delivery/tests/fixtures/delivery-main.json"
        ))
        .expect("canonical fixture");
        let binding_id = aggregate.snapshot().session_bindings[0].id.clone();
        let binding = accepted_binding(
            &aggregate,
            &SessionBindingId::new(binding_id.0).expect("binding id"),
            RuntimeAuthorityFixture::default(),
            Some(1),
        )
        .expect("accepted binding");
        let event = accepted_event(
            &binding,
            1,
            "runtime-checkpoint",
            RuntimeFactFixture::Checkpoint,
        )
        .expect("checkpoint");
        let mut projection = RuntimeProjection::new(&aggregate, vec![binding]).expect("projection");
        projection.apply(&event).expect("accepted checkpoint");
        projection
    }

    #[test]
    fn trusted_runtime_read_rejects_sequence_behind_fold() {
        let projection = runtime_projection();
        assert_eq!(projection.snapshot().sessions[0].as_of_sequence, 1);
        let read = TrustedRuntimeProjectionRead::try_new(
            scope(),
            7,
            Revision(1),
            1,
            Instant("2026-08-25T00:00:00Z".into()),
            &projection,
            Sha256Digest(format!("sha256:{}", "a".repeat(64))),
        )
        .expect("bounded read");
        assert_eq!(
            read.snapshot().delivery_id.as_ref(),
            Some(&projection.snapshot().delivery_id)
        );
        assert_eq!(read.snapshot().sessions, projection.snapshot().sessions);

        assert_eq!(
            TrustedRuntimeProjectionRead::try_new(
                scope(),
                7,
                Revision(1),
                0,
                Instant("2026-08-25T00:00:00Z".into()),
                &projection,
                Sha256Digest(format!("sha256:{}", "a".repeat(64))),
            )
            .expect_err("accepted cursor cannot trail its fold"),
            TrustedProjectionReadError::Invalid
        );
    }

    struct FixedReadCut {
        cut: TrustedRuntimeProjectionReadCut,
    }

    impl TrustedRuntimeProjectionReadCutReader for FixedReadCut {
        fn read_delivery_cut(
            &self,
            _storage: &dyn ProductStateStorage,
            _request: &DeliveryRuntimeReadRequest,
            _delivery: &Delivery,
        ) -> Result<TrustedRuntimeProjectionReadCut, TrustedProjectionReadError> {
            Ok(self.cut.clone())
        }

        fn read_product_session_cut(
            &self,
            _storage: &dyn ProductStateStorage,
            _request: &ProductSessionRuntimeReadRequest,
        ) -> Result<TrustedRuntimeProjectionReadCut, TrustedProjectionReadError> {
            Ok(self.cut.clone())
        }
    }

    #[test]
    fn sqlite_storage_product_session_read_cut_rebuilds_runtime_projection() {
        let root = std::env::temp_dir().join(format!(
            "winwincode-control-plane-product-source-test-{}",
            std::process::id()
        ));
        let mut storage = winwincode_storage::SqliteStorage::open(&root).expect("SQLite storage");
        let scope = canonical_source_scope();
        let product_session_id = ProductSessionId("psn_01J00000000000000000000000".into());
        let stream_id = product_session_runtime_stream_id_for_projection(&product_session_id);
        let (event, event_digest) = source_runtime_event("xevt_01J00000000000000000000000");
        let lease = ExecutionLeaseStamp {
            attempt: 1,
            expires_at: DomainInstant("2026-08-25T01:00:00Z".into()),
            fencing_token: FencingToken("1".into()),
            issued_at: DomainInstant("2026-08-25T00:00:00Z".into()),
            job_id: ExecutionJobId("job_01J00000000000000000000000".into()),
            lease_id: LeaseId("lse_01J00000000000000000000000".into()),
            worker_id: WorkerId("wrk_01J00000000000000000000000".into()),
            worker_instance_id: WorkerInstanceId("wki_01J00000000000000000000000".into()),
        };
        let ledger = RuntimeLedgerState {
            schema_version: 1,
            delivery_id: None,
            delivery_task_id: None,
            stage_run_id: None,
            product_session_id: product_session_id.clone(),
            execution_job_id: lease.job_id.clone(),
            worker_session_id: WorkerSessionId("wsn_01J00000000000000000000000".into()),
            codex_thread_id: CodexThreadId("cdx_01J00000000000000000000000".into()),
            lease_id: lease.lease_id.clone(),
            attempt: 1,
            fencing_token: lease.fencing_token.clone(),
            worker_id: lease.worker_id.clone(),
            worker_instance_id: lease.worker_instance_id.clone(),
            highest_sequence: 1,
            events: vec![RuntimeLedgerEvent {
                event,
                event_digest,
            }],
        };
        commit_runtime_fixture(
            &mut storage,
            &scope,
            RuntimeFixtureCommit {
                request_id: "req_source_product_fixture_0001",
                command_digest: 'b',
                stream_id,
                ledger: &ledger,
                event_id: "evt_product_session_fixture_0001",
                stream: ProjectionEventStream::ProductSession(product_session_id.clone()),
            },
        );

        let request = ProductSessionRuntimeReadRequest::new(scope, product_session_id, None, 20);
        let read = SqliteStorageRuntimeProjectionReadCutReader
            .read_product_session_cut(&storage, &request)
            .expect("ProductSession runtime cut");
        assert_eq!(read.event_cursor().sequence(), 1);
        assert_eq!(read.runtime().delivery_revision(), 0);
        assert_eq!(read.runtime().ledger_revision(), &Revision(1));
        assert_eq!(read.runtime().accepted_sequence(), 1);
        assert!(read.runtime().snapshot().delivery_id.is_none());
        assert!(
            read.runtime()
                .snapshot()
                .product_session_id
                .as_ref()
                .is_some_and(|id| id == request.product_session_id())
        );
        assert!(read.runtime().snapshot().sessions.is_empty());
        Box::new(storage).close().expect("SQLite close");
        std::fs::remove_dir_all(root).expect("temporary source directory");
    }

    #[test]
    fn sqlite_product_session_read_cut_is_empty_before_the_first_runtime_event() {
        let root = std::env::temp_dir().join(format!(
            "winwincode-control-plane-empty-product-source-test-{}",
            std::process::id()
        ));
        let mut storage = winwincode_storage::SqliteStorage::open(&root).expect("SQLite storage");
        let scope = canonical_source_scope();
        let product_session_id = ProductSessionId("psn_01J00000000000000000000001".into());
        let actor = PublicEventActor::System {
            id: winwincode_domain::SystemActorId("sys_01J00000000000000000000000".into()),
        };
        let receipt_identity = ReceiptIdentity::new(
            receipt_actor_key(&actor).expect("actor key"),
            crate::repository_scope_key(&scope).expect("scope key"),
            RequestId("req_empty_product_runtime_0001".into()),
        )
        .expect("receipt identity");
        let event = ControlPlaneWebSocketProductSessionRuntimeProjectionInvalidatedEvent {
            last_projection_sequence: 0,
            product_session_id: product_session_id.clone(),
            projection_revision: Revision(0),
            reload_queries: (
                ControlPlaneWebSocketRuntimeProjectionGetReloadQuery::RuntimeProjectionGet,
            ),
            scope_kind:
                ControlPlaneWebSocketProductSessionRuntimeProjectionInvalidatedEventScopeKind::ProductSession,
            type_value:
                ControlPlaneWebSocketProductSessionRuntimeProjectionInvalidatedEventTypeValue::RuntimeProjectionInvalidatedV1,
        };
        storage
            .commit(&StateCommit::new(
                receipt_identity,
                Sha256Digest(format!("sha256:{}", "d".repeat(64))),
                "unrelated:empty-product-runtime",
                0,
                b"{}".to_vec(),
                vec![
                    NewOutboxEvent::public_projection(
                        ControlPlaneEventId("evt_empty_product_runtime_0001".into()),
                        "runtime-projection.invalidated.v1",
                        serde_json::to_vec(&event).expect("generated event"),
                        ProjectionEventStream::ProductSession(product_session_id.clone()),
                        crate::public_repository_scope(&scope),
                        DomainInstant("2026-08-25T00:00:00.000Z".into()),
                        PublicEventSource::ControlPlane {
                            actor,
                            component: "empty-product-runtime-fixture".into(),
                        },
                    )
                    .expect("public event"),
                ],
            ))
            .expect("public event commit");

        let request = ProductSessionRuntimeReadRequest::new(scope, product_session_id, None, 20);
        let read = SqliteStorageRuntimeProjectionReadCutReader
            .read_product_session_cut(&storage, &request)
            .expect("empty ProductSession runtime cut");
        assert_eq!(read.event_cursor().sequence(), 1);
        assert_eq!(read.runtime().ledger_revision(), &Revision(0));
        assert_eq!(read.runtime().accepted_sequence(), 0);
        assert_eq!(read.runtime().rebuilt_at().0, "1970-01-01T00:00:00.000Z");
        assert!(read.runtime().snapshot().sessions.is_empty());
        Box::new(storage).close().expect("SQLite close");
        std::fs::remove_dir_all(root).expect("temporary source directory");
    }

    #[test]
    fn sqlite_storage_delivery_read_cut_rebuilds_runtime_projection() {
        let root = std::env::temp_dir().join(format!(
            "winwincode-control-plane-delivery-source-test-{}",
            std::process::id()
        ));
        let mut storage = winwincode_storage::SqliteStorage::open(&root).expect("SQLite storage");
        let scope = canonical_source_scope();
        let scope_key = crate::repository_scope_key(&scope).expect("scope key");
        let delivery = Delivery::decode_json(include_bytes!(
            "../../../winwincode-delivery/tests/fixtures/delivery-main.json"
        ))
        .expect("canonical fixture");
        let binding = &delivery.snapshot().session_bindings[0];
        let worker_session_id = binding
            .worker_session_id
            .clone()
            .expect("fixture WorkerSession");
        let codex_thread_id = binding
            .codex_thread_id
            .clone()
            .expect("fixture CodexThread");
        let lease_id = binding.lease_id.clone().expect("fixture lease");
        let fencing_token = binding
            .fencing_token
            .clone()
            .expect("fixture fencing token");
        let worker_id = binding.worker_id.clone().expect("fixture Worker");
        let worker_instance_id = binding
            .worker_instance_id
            .clone()
            .expect("fixture Worker instance");
        let stream_id = runtime_stream_id_for_projection(&scope_key, &binding.execution_job_id);
        let (event, event_digest) =
            source_runtime_event("xevt_delivery_01J00000000000000000000000");
        let ledger = RuntimeLedgerState {
            schema_version: 1,
            delivery_id: Some(delivery.id().clone()),
            delivery_task_id: binding.delivery_task_id.clone(),
            stage_run_id: Some(binding.stage_run_id.clone()),
            product_session_id: binding.product_session_id.clone(),
            execution_job_id: binding.execution_job_id.clone(),
            worker_session_id,
            codex_thread_id,
            lease_id,
            attempt: binding.attempt,
            fencing_token,
            worker_id,
            worker_instance_id,
            highest_sequence: 1,
            events: vec![RuntimeLedgerEvent {
                event,
                event_digest,
            }],
        };
        commit_runtime_fixture(
            &mut storage,
            &scope,
            RuntimeFixtureCommit {
                request_id: "req_source_delivery_fixture_0001",
                command_digest: 'c',
                stream_id,
                ledger: &ledger,
                event_id: "evt_delivery_fixture_0001",
                stream: ProjectionEventStream::Delivery(delivery.id().clone()),
            },
        );

        let request = DeliveryRuntimeReadRequest::new(
            scope,
            delivery.id().clone(),
            delivery.revision(),
            None,
            20,
        );
        let read = SqliteStorageRuntimeProjectionReadCutReader
            .read_delivery_cut(&storage, &request, &delivery)
            .expect("Delivery runtime cut");
        assert_eq!(read.event_cursor().sequence(), 1);
        assert_eq!(read.runtime().delivery_revision(), delivery.revision());
        assert_eq!(read.runtime().ledger_revision(), &Revision(1));
        assert_eq!(read.runtime().accepted_sequence(), 1);
        assert_eq!(
            read.runtime().snapshot().delivery_id.as_ref(),
            Some(delivery.id())
        );
        assert!(read.runtime().snapshot().product_session_id.is_none());
        assert_eq!(read.runtime().snapshot().sessions.len(), 1);
        assert_eq!(read.runtime().snapshot().sessions[0].as_of_sequence, 1);
        Box::new(storage).close().expect("SQLite close");
        std::fs::remove_dir_all(root).expect("temporary source directory");
    }

    #[test]
    fn sqlite_adapter_returns_runtime_and_cursor_from_one_read_cut() {
        let runtime = TrustedRuntimeProjectionRead::try_new(
            scope(),
            7,
            Revision(1),
            1,
            Instant("2026-08-25T00:00:00Z".into()),
            &runtime_projection(),
            Sha256Digest(format!("sha256:{}", "a".repeat(64))),
        )
        .expect("trusted runtime");
        let stream =
            ProjectionEventStream::Delivery(DeliveryId("dlv_01J00000000000000000000000".into()));
        let key = ProjectionEventStreamKey::new(
            ReceiptScopeKey::from_encoded(b"fixture-scope".to_vec()).expect("scope key"),
            stream,
        )
        .expect("stream key");
        let cursor = ProjectionEventCursor::try_new(
            key,
            1,
            Some(ControlPlaneEventId("evt_01J00000000000000000000000".into())),
        )
        .expect("event cursor");
        let cut = TrustedRuntimeProjectionReadCut::new(runtime, cursor);
        let adapter =
            SqliteTrustedRuntimeProjectionAdapter::new(Box::new(FixedReadCut { cut: cut.clone() }));
        let root = std::env::temp_dir().join(format!(
            "winwincode-control-plane-source-test-{}",
            std::process::id()
        ));
        let storage = winwincode_storage::SqliteStorage::open(&root).expect("SQLite storage");
        let delivery = Delivery::decode_json(include_bytes!(
            "../../../winwincode-delivery/tests/fixtures/delivery-main.json"
        ))
        .expect("canonical fixture");

        assert!(adapter.provides_atomic_read_cut());
        let delivery_read = adapter
            .read_delivery_with_storage(
                &storage,
                &DeliveryRuntimeReadRequest::new(
                    scope(),
                    DeliveryId("dlv_01J00000000000000000000000".into()),
                    7,
                    None,
                    20,
                ),
                &delivery,
            )
            .expect("Delivery cut");
        assert_eq!(delivery_read.event_cursor(), Some(cut.event_cursor()));

        let product_read = adapter
            .read_product_session_with_storage(
                &storage,
                &ProductSessionRuntimeReadRequest::new(
                    scope(),
                    ProductSessionId("product:fixture".into()),
                    None,
                    20,
                ),
            )
            .expect("ProductSession cut");
        assert_eq!(product_read.event_cursor(), Some(cut.event_cursor()));
        Box::new(storage).close().expect("SQLite close");
        std::fs::remove_dir_all(root).expect("temporary source directory");
    }
}
