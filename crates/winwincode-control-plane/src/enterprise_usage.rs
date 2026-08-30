// SPDX-License-Identifier: Apache-2.0

//! Rebuildable enterprise Usage projections from immutable settled sources.

use std::fmt;

use winwincode_domain::PublicationId;
use winwincode_publication::{
    PublicationMeteringCursor, PublicationMeteringError, PublicationMeteringFilter,
    PublicationMeteringLedger, PublicationMeteringSourceEntry,
};
use winwincode_storage::{
    ArtifactError, ArtifactStorageSourceCursor, ArtifactStorageSourceEntry, ArtifactStore,
    EnterpriseQuotaError, EnterpriseUsageAttribution, EnterpriseUsageError, EnterpriseUsageMeasure,
    EnterpriseUsageSource, ExecutionAdmissionError, SettledEnterpriseUsage, SqliteStorage,
    WorkerSettlementSourceCursor, WorkerSettlementSourceEntry,
};

use crate::{
    ModelRetryUsageError, ModelRetryUsageService, ModelUsageFilter, ModelUsageSourceCursor,
    ModelUsageSourceEntry,
};

/// One bounded reconciliation result from the Provider source catalog.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderEnterpriseUsageReconciliation {
    pub snapshot_sequence: u64,
    pub source_entries: u64,
    pub inserted_entries: u64,
    pub replayed_entries: u64,
    pub next: Option<ModelUsageSourceCursor>,
}

/// Stable failure categories for Provider-to-enterprise reconciliation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderEnterpriseUsageErrorKind {
    Source,
    Ledger,
}

/// Bounded error that does not retain Provider or model content.
#[derive(Debug)]
pub struct ProviderEnterpriseUsageError {
    kind: ProviderEnterpriseUsageErrorKind,
}

impl ProviderEnterpriseUsageError {
    const fn new(kind: ProviderEnterpriseUsageErrorKind) -> Self {
        Self { kind }
    }

    #[must_use]
    pub const fn kind(&self) -> ProviderEnterpriseUsageErrorKind {
        self.kind
    }
}

impl fmt::Display for ProviderEnterpriseUsageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Provider enterprise Usage reconciliation failed")
    }
}

impl std::error::Error for ProviderEnterpriseUsageError {}

impl From<ModelRetryUsageError> for ProviderEnterpriseUsageError {
    fn from(_error: ModelRetryUsageError) -> Self {
        Self::new(ProviderEnterpriseUsageErrorKind::Source)
    }
}

impl From<EnterpriseUsageError> for ProviderEnterpriseUsageError {
    fn from(_error: EnterpriseUsageError) -> Self {
        Self::new(ProviderEnterpriseUsageErrorKind::Ledger)
    }
}

impl From<EnterpriseQuotaError> for ProviderEnterpriseUsageError {
    fn from(_error: EnterpriseQuotaError) -> Self {
        Self::new(ProviderEnterpriseUsageErrorKind::Ledger)
    }
}

/// Canonical Provider source-catalog projector over one durable database.
pub struct ProviderEnterpriseUsageReconciler<'storage> {
    storage: &'storage mut SqliteStorage,
}

impl<'storage> ProviderEnterpriseUsageReconciler<'storage> {
    #[must_use]
    pub const fn new(storage: &'storage mut SqliteStorage) -> Self {
        Self { storage }
    }

    /// Projects one bounded fixed-snapshot source page into the enterprise ledger.
    ///
    /// Source settlement is already durable before this call. If the process
    /// stops between the two stores, restarting the scan from the beginning is
    /// safe because the enterprise ledger replays the source receipt exactly.
    ///
    /// # Errors
    ///
    /// Rejects corrupt/mismatched source facts, changed cursors, invalid limits,
    /// or unavailable durable storage.
    pub fn reconcile_provider_page(
        &mut self,
        cursor: Option<&ModelUsageSourceCursor>,
        limit: u64,
    ) -> Result<ProviderEnterpriseUsageReconciliation, ProviderEnterpriseUsageError> {
        let filter = ModelUsageFilter::default();
        let page = ModelRetryUsageService::new(&mut *self.storage)
            .scan_usage_sources(&filter, cursor, limit)?;
        let mut inserted_entries = 0_u64;
        let mut replayed_entries = 0_u64;
        for source in &page.entries {
            let fact = provider_fact(source);
            let receipt = self.storage.enterprise_usage_ledger()?.record(&fact)?;
            self.storage
                .enterprise_quota_ledger()?
                .settle_usage_source(&fact.source)?;
            if receipt.idempotent_replay {
                replayed_entries = checked_increment(replayed_entries)?;
            } else {
                inserted_entries = checked_increment(inserted_entries)?;
            }
        }
        Ok(ProviderEnterpriseUsageReconciliation {
            snapshot_sequence: page.snapshot_sequence,
            source_entries: u64::try_from(page.entries.len()).map_err(|_| {
                ProviderEnterpriseUsageError::new(ProviderEnterpriseUsageErrorKind::Source)
            })?,
            inserted_entries,
            replayed_entries,
            next: page.next,
        })
    }
}

fn provider_fact(source: &ModelUsageSourceEntry) -> SettledEnterpriseUsage {
    SettledEnterpriseUsage {
        source: EnterpriseUsageSource::Provider {
            provider_usage_id: source.usage.provider_usage_id.clone(),
            source_sequence: source.sequence,
            source_digest: source.source_digest.clone(),
            model_exchange_id: source.model_exchange_id.clone(),
            request_id: source.request_id.clone(),
            attempt: source.usage.attempt,
            route_authority_fingerprint: source.route_authority_fingerprint.clone(),
        },
        attribution: EnterpriseUsageAttribution {
            organization_id: source.attribution.organization_id.clone(),
            workspace_id: source.attribution.workspace_id.clone(),
            project_id: source.attribution.project_id.clone(),
            repository_id: source.attribution.repository_id.clone(),
            delivery_id: source.attribution.delivery_id.clone(),
            product_session_id: Some(source.attribution.product_session_id.clone()),
            user_id: source.attribution.user_id.clone(),
        },
        measure: EnterpriseUsageMeasure::Provider {
            input_tokens: source.usage.input_tokens,
            cached_input_tokens: source.usage.cached_input_tokens,
            cache_write_input_tokens: source.usage.cache_write_input_tokens,
            output_tokens: source.usage.output_tokens,
            reasoning_output_tokens: source.usage.reasoning_output_tokens,
            total_tokens: source.usage.total_tokens,
            cost_micros: source.usage.cost_micros,
        },
        settled_at: source.settled_at.clone(),
    }
}

fn checked_increment(value: u64) -> Result<u64, ProviderEnterpriseUsageError> {
    value
        .checked_add(1)
        .ok_or_else(|| ProviderEnterpriseUsageError::new(ProviderEnterpriseUsageErrorKind::Ledger))
}

/// One bounded reconciliation result from the worker settlement-source catalog.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerEnterpriseUsageReconciliation {
    pub snapshot_sequence: u64,
    pub source_entries: u64,
    pub inserted_entries: u64,
    pub replayed_entries: u64,
    pub next: Option<WorkerSettlementSourceCursor>,
}

/// Stable failure categories for worker-to-enterprise reconciliation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerEnterpriseUsageErrorKind {
    Source,
    Ledger,
}

/// Bounded error that does not retain execution identities or usage values.
#[derive(Debug)]
pub struct WorkerEnterpriseUsageError {
    kind: WorkerEnterpriseUsageErrorKind,
}

impl WorkerEnterpriseUsageError {
    const fn new(kind: WorkerEnterpriseUsageErrorKind) -> Self {
        Self { kind }
    }

    #[must_use]
    pub const fn kind(&self) -> WorkerEnterpriseUsageErrorKind {
        self.kind
    }
}

impl fmt::Display for WorkerEnterpriseUsageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("worker enterprise Usage reconciliation failed")
    }
}

impl std::error::Error for WorkerEnterpriseUsageError {}

impl From<ExecutionAdmissionError> for WorkerEnterpriseUsageError {
    fn from(_error: ExecutionAdmissionError) -> Self {
        Self::new(WorkerEnterpriseUsageErrorKind::Source)
    }
}

impl From<EnterpriseUsageError> for WorkerEnterpriseUsageError {
    fn from(_error: EnterpriseUsageError) -> Self {
        Self::new(WorkerEnterpriseUsageErrorKind::Ledger)
    }
}

impl From<EnterpriseQuotaError> for WorkerEnterpriseUsageError {
    fn from(_error: EnterpriseQuotaError) -> Self {
        Self::new(WorkerEnterpriseUsageErrorKind::Ledger)
    }
}

/// Canonical worker settlement-source projector into one durable Usage ledger.
pub struct WorkerEnterpriseUsageReconciler<'storage> {
    storage: &'storage mut SqliteStorage,
}

impl<'storage> WorkerEnterpriseUsageReconciler<'storage> {
    #[must_use]
    pub const fn new(storage: &'storage mut SqliteStorage) -> Self {
        Self { storage }
    }

    /// Projects one bounded fixed-snapshot worker source page into the ledger.
    ///
    /// # Errors
    ///
    /// Rejects corrupt source facts, invalid cursors/limits, changed source
    /// identities, or unavailable durable storage.
    pub fn reconcile_worker_page(
        &mut self,
        cursor: Option<&WorkerSettlementSourceCursor>,
        limit: u64,
    ) -> Result<WorkerEnterpriseUsageReconciliation, WorkerEnterpriseUsageError> {
        let page = self
            .storage
            .execution_admission()?
            .scan_settlement_sources(cursor, limit)?;
        let mut inserted_entries = 0_u64;
        let mut replayed_entries = 0_u64;
        for source in &page.entries {
            let fact = worker_fact(source);
            let receipt = self.storage.enterprise_usage_ledger()?.record(&fact)?;
            self.storage
                .enterprise_quota_ledger()?
                .settle_usage_source(&fact.source)?;
            if receipt.idempotent_replay {
                replayed_entries = worker_checked_increment(replayed_entries)?;
            } else {
                inserted_entries = worker_checked_increment(inserted_entries)?;
            }
        }
        Ok(WorkerEnterpriseUsageReconciliation {
            snapshot_sequence: page.snapshot_sequence,
            source_entries: u64::try_from(page.entries.len()).map_err(|_| {
                WorkerEnterpriseUsageError::new(WorkerEnterpriseUsageErrorKind::Source)
            })?,
            inserted_entries,
            replayed_entries,
            next: page.next,
        })
    }
}

fn worker_fact(source: &WorkerSettlementSourceEntry) -> SettledEnterpriseUsage {
    SettledEnterpriseUsage {
        source: EnterpriseUsageSource::Worker {
            job_id: source.fact.job_id.clone(),
            settlement_request_id: source.fact.settlement_request_id.clone(),
            worker_pool_id: source.fact.worker_pool_id.0.clone(),
        },
        attribution: EnterpriseUsageAttribution {
            organization_id: source.fact.scope.organization_id.clone(),
            workspace_id: source.fact.scope.workspace_id.clone(),
            project_id: source.fact.scope.project_id.clone(),
            repository_id: source.fact.scope.repository_id.clone(),
            delivery_id: source.fact.scope.delivery_id.clone(),
            product_session_id: Some(source.fact.scope.product_session_id.clone()),
            user_id: source.fact.user_id.clone(),
        },
        measure: EnterpriseUsageMeasure::Worker {
            runtime_millis: source.fact.actual_runtime_millis,
            tokens: source.fact.actual_tokens,
            cost_microunits: source.fact.actual_cost_microunits,
        },
        settled_at: source.fact.completed_at.clone(),
    }
}

fn worker_checked_increment(value: u64) -> Result<u64, WorkerEnterpriseUsageError> {
    value
        .checked_add(1)
        .ok_or_else(|| WorkerEnterpriseUsageError::new(WorkerEnterpriseUsageErrorKind::Ledger))
}

/// One bounded reconciliation result from the Artifact storage-source catalog.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StorageEnterpriseUsageReconciliation {
    pub snapshot_sequence: u64,
    pub source_entries: u64,
    pub inserted_entries: u64,
    pub replayed_entries: u64,
    pub next: Option<ArtifactStorageSourceCursor>,
}

/// Stable failure categories for storage-to-enterprise reconciliation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StorageEnterpriseUsageErrorKind {
    Source,
    Ledger,
}

/// Bounded error that does not retain Artifact metadata or source facts.
#[derive(Debug)]
pub struct StorageEnterpriseUsageError {
    kind: StorageEnterpriseUsageErrorKind,
}

impl StorageEnterpriseUsageError {
    const fn new(kind: StorageEnterpriseUsageErrorKind) -> Self {
        Self { kind }
    }

    #[must_use]
    pub const fn kind(&self) -> StorageEnterpriseUsageErrorKind {
        self.kind
    }
}

impl fmt::Display for StorageEnterpriseUsageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("storage enterprise Usage reconciliation failed")
    }
}

impl std::error::Error for StorageEnterpriseUsageError {}

impl From<ArtifactError> for StorageEnterpriseUsageError {
    fn from(_error: ArtifactError) -> Self {
        Self::new(StorageEnterpriseUsageErrorKind::Source)
    }
}

impl From<EnterpriseUsageError> for StorageEnterpriseUsageError {
    fn from(_error: EnterpriseUsageError) -> Self {
        Self::new(StorageEnterpriseUsageErrorKind::Ledger)
    }
}

impl From<EnterpriseQuotaError> for StorageEnterpriseUsageError {
    fn from(_error: EnterpriseQuotaError) -> Self {
        Self::new(StorageEnterpriseUsageErrorKind::Ledger)
    }
}

/// Canonical Artifact source-catalog projector into one durable Usage ledger.
pub struct StorageEnterpriseUsageReconciler<'storage, 'artifacts> {
    storage: &'storage mut SqliteStorage,
    artifacts: &'artifacts ArtifactStore,
}

impl<'storage, 'artifacts> StorageEnterpriseUsageReconciler<'storage, 'artifacts> {
    #[must_use]
    pub const fn new(
        storage: &'storage mut SqliteStorage,
        artifacts: &'artifacts ArtifactStore,
    ) -> Self {
        Self { storage, artifacts }
    }

    /// Projects one bounded fixed-snapshot storage-source page into the ledger.
    ///
    /// # Errors
    ///
    /// Rejects corrupt source facts, changed cursors, invalid limits, source
    /// identity conflicts, or unavailable durable storage.
    pub fn reconcile_storage_page(
        &mut self,
        cursor: Option<&ArtifactStorageSourceCursor>,
        limit: u64,
    ) -> Result<StorageEnterpriseUsageReconciliation, StorageEnterpriseUsageError> {
        let page = self.artifacts.scan_storage_sources(cursor, limit)?;
        let mut inserted_entries = 0_u64;
        let mut replayed_entries = 0_u64;
        for source in &page.entries {
            let fact = storage_fact(source);
            let receipt = self.storage.enterprise_usage_ledger()?.record(&fact)?;
            self.storage
                .enterprise_quota_ledger()?
                .settle_usage_source(&fact.source)?;
            if receipt.idempotent_replay {
                replayed_entries = storage_checked_increment(replayed_entries)?;
            } else {
                inserted_entries = storage_checked_increment(inserted_entries)?;
            }
        }
        Ok(StorageEnterpriseUsageReconciliation {
            snapshot_sequence: page.snapshot_sequence,
            source_entries: u64::try_from(page.entries.len()).map_err(|_| {
                StorageEnterpriseUsageError::new(StorageEnterpriseUsageErrorKind::Source)
            })?,
            inserted_entries,
            replayed_entries,
            next: page.next,
        })
    }
}

fn storage_fact(source: &ArtifactStorageSourceEntry) -> SettledEnterpriseUsage {
    SettledEnterpriseUsage {
        source: EnterpriseUsageSource::Storage {
            operation_id: source.fact.operation_id.clone(),
            source_sequence: source.sequence,
            source_digest: source.source_digest.clone(),
            artifact_id: source.fact.artifact_id.clone(),
            operation_kind: source.fact.operation_kind,
            request_id: source.fact.request_id.clone(),
        },
        attribution: EnterpriseUsageAttribution {
            organization_id: source.fact.attribution.organization_id.clone(),
            workspace_id: source.fact.attribution.workspace_id.clone(),
            project_id: source.fact.attribution.project_id.clone(),
            repository_id: source.fact.attribution.repository_id.clone(),
            delivery_id: source.fact.attribution.delivery_id.clone(),
            product_session_id: source.fact.attribution.product_session_id.clone(),
            user_id: source.fact.attribution.user_id.clone(),
        },
        measure: EnterpriseUsageMeasure::Storage {
            bytes: source.fact.bytes,
        },
        settled_at: source.fact.occurred_at.clone(),
    }
}

fn storage_checked_increment(value: u64) -> Result<u64, StorageEnterpriseUsageError> {
    value
        .checked_add(1)
        .ok_or_else(|| StorageEnterpriseUsageError::new(StorageEnterpriseUsageErrorKind::Ledger))
}

/// One bounded reconciliation result from the Publication provider-write catalog.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublicationEnterpriseUsageReconciliation {
    pub snapshot_sequence: u64,
    pub source_entries: u64,
    pub inserted_entries: u64,
    pub replayed_entries: u64,
    pub next: Option<PublicationMeteringCursor>,
}

/// Stable failure categories for Publication-to-enterprise reconciliation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublicationEnterpriseUsageErrorKind {
    Source,
    Ledger,
}

/// Bounded error that does not retain provider payloads or credentials.
#[derive(Debug)]
pub struct PublicationEnterpriseUsageError {
    kind: PublicationEnterpriseUsageErrorKind,
}

impl PublicationEnterpriseUsageError {
    const fn new(kind: PublicationEnterpriseUsageErrorKind) -> Self {
        Self { kind }
    }

    #[must_use]
    pub const fn kind(&self) -> PublicationEnterpriseUsageErrorKind {
        self.kind
    }
}

impl fmt::Display for PublicationEnterpriseUsageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Publication enterprise Usage reconciliation failed")
    }
}

impl std::error::Error for PublicationEnterpriseUsageError {}

impl From<PublicationMeteringError> for PublicationEnterpriseUsageError {
    fn from(_error: PublicationMeteringError) -> Self {
        Self::new(PublicationEnterpriseUsageErrorKind::Source)
    }
}

impl From<EnterpriseUsageError> for PublicationEnterpriseUsageError {
    fn from(_error: EnterpriseUsageError) -> Self {
        Self::new(PublicationEnterpriseUsageErrorKind::Ledger)
    }
}

impl From<EnterpriseQuotaError> for PublicationEnterpriseUsageError {
    fn from(_error: EnterpriseQuotaError) -> Self {
        Self::new(PublicationEnterpriseUsageErrorKind::Ledger)
    }
}

/// Canonical Publication source-catalog projector into one durable Usage ledger.
pub struct PublicationEnterpriseUsageReconciler<'storage> {
    storage: &'storage mut SqliteStorage,
}

impl<'storage> PublicationEnterpriseUsageReconciler<'storage> {
    #[must_use]
    pub const fn new(storage: &'storage mut SqliteStorage) -> Self {
        Self { storage }
    }

    /// Projects one bounded fixed-snapshot Publication source page into the ledger.
    ///
    /// A restart can safely resume the cursor or restart from the beginning:
    /// the enterprise ledger uses the immutable Publication operation identity
    /// as its idempotency authority.
    ///
    /// # Errors
    ///
    /// Rejects corrupt source facts, changed cursors, invalid limits, source
    /// identity conflicts, or unavailable durable storage.
    pub fn reconcile_publication_page(
        &mut self,
        cursor: Option<&PublicationMeteringCursor>,
        limit: u64,
    ) -> Result<PublicationEnterpriseUsageReconciliation, PublicationEnterpriseUsageError> {
        self.reconcile(&PublicationMeteringFilter::default(), cursor, limit)
    }

    /// Projects and settles every source for one exact Publication.
    ///
    /// A Publication has at most four ordered remote operations. The bounded
    /// scan therefore rejects a continuation instead of silently leaving a
    /// successful provider effect unprojected.
    ///
    /// # Errors
    ///
    /// Rejects corrupt source facts, a source count beyond the Publication
    /// operation bound, or unavailable durable storage.
    pub fn reconcile_exact_publication(
        &mut self,
        publication_id: &PublicationId,
    ) -> Result<PublicationEnterpriseUsageReconciliation, PublicationEnterpriseUsageError> {
        let reconciliation = self.reconcile(
            &PublicationMeteringFilter {
                publication_id: Some(publication_id.clone()),
                ..PublicationMeteringFilter::default()
            },
            None,
            16,
        )?;
        if reconciliation.next.is_some() || reconciliation.source_entries > 4 {
            return Err(PublicationEnterpriseUsageError::new(
                PublicationEnterpriseUsageErrorKind::Source,
            ));
        }
        Ok(reconciliation)
    }

    fn reconcile(
        &mut self,
        filter: &PublicationMeteringFilter,
        cursor: Option<&PublicationMeteringCursor>,
        limit: u64,
    ) -> Result<PublicationEnterpriseUsageReconciliation, PublicationEnterpriseUsageError> {
        let page =
            PublicationMeteringLedger::new(&*self.storage).scan_sources(filter, cursor, limit)?;
        let mut inserted_entries = 0_u64;
        let mut replayed_entries = 0_u64;
        for source in &page.entries {
            let fact = publication_fact(source);
            let receipt = self.storage.enterprise_usage_ledger()?.record(&fact)?;
            self.storage
                .enterprise_quota_ledger()?
                .settle_usage_source(&fact.source)?;
            if receipt.idempotent_replay {
                replayed_entries = publication_checked_increment(replayed_entries)?;
            } else {
                inserted_entries = publication_checked_increment(inserted_entries)?;
            }
        }
        Ok(PublicationEnterpriseUsageReconciliation {
            snapshot_sequence: page.snapshot_sequence,
            source_entries: u64::try_from(page.entries.len()).map_err(|_| {
                PublicationEnterpriseUsageError::new(PublicationEnterpriseUsageErrorKind::Source)
            })?,
            inserted_entries,
            replayed_entries,
            next: page.next,
        })
    }
}

fn publication_fact(source: &PublicationMeteringSourceEntry) -> SettledEnterpriseUsage {
    SettledEnterpriseUsage {
        source: EnterpriseUsageSource::Publication {
            publication_id: source.publication_id.clone(),
            operation_key: source.operation_key.clone(),
            request_sha256: source.request_sha256.clone(),
        },
        attribution: EnterpriseUsageAttribution {
            organization_id: source.attribution.organization_id().clone(),
            workspace_id: source.attribution.workspace_id().clone(),
            project_id: source.attribution.project_id().clone(),
            repository_id: source.attribution.repository_id().clone(),
            delivery_id: Some(source.attribution.delivery_id().clone()),
            product_session_id: Some(source.attribution.product_session_id().clone()),
            user_id: source.attribution.user_id().clone(),
        },
        measure: EnterpriseUsageMeasure::Publication,
        settled_at: source.occurred_at.clone(),
    }
}

fn publication_checked_increment(value: u64) -> Result<u64, PublicationEnterpriseUsageError> {
    value.checked_add(1).ok_or_else(|| {
        PublicationEnterpriseUsageError::new(PublicationEnterpriseUsageErrorKind::Ledger)
    })
}
