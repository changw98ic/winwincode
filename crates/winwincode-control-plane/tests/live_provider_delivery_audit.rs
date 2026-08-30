// SPDX-License-Identifier: Apache-2.0

//! Independent, read-mostly audit of one completed live Provider Delivery.
//!
//! The live runner writes only identifiers and expected numeric Usage to a
//! mode-0600 evidence file. This gate reopens the same product database and
//! joins its existing Delivery, Provider, admission, pool, slot, lease, and
//! enterprise Usage facts. It creates no parallel receipt or billing ledger.

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
use std::{
    collections::BTreeSet,
    error::Error,
    fmt, fs,
    io::{self, Read},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use winwincode_api::generated::{RepositoryScope, RepositoryScopeKind};
use winwincode_control_plane::{
    DurableModelExchangeAuthority, FrozenModelRouteAuthority, ModelAdmissionClock,
    ModelAdmissionClockError, ModelAdmissionService, ModelRequestPool, ModelRequestPoolConfig,
    ModelReservationTerminalOutcome, ModelRetryUsageService, ModelUsageFilter,
    ProviderGatewayTerminalOutcome, ProviderGatewayTerminalProgressPort,
    ProviderGatewayTerminalProgressStage, load_delivery_authority_seal,
};
use winwincode_delivery::domain::DeliveryStatus;
use winwincode_delivery::store::{
    DeliveryJournalCodec, DeliveryMutationOperation, JournalEntryState, JournalRecordBytes,
    LoadedDeliveryJournal,
};
use winwincode_domain::{
    DeliveryId, ExecutionJobId, Instant, LeaseId, ModelExchangeId, RequestId, WorkerSessionId,
};
use winwincode_storage::{
    AggregateJournalKey, EnterpriseUsageMeasure, EnterpriseUsageSource,
    ExecutionLeaseTerminalOutcome, ExecutionLeaseTerminalRequest, ProductStateStorage,
    ProviderExchangeState, SqliteStorage, WorkerSlotResources, WorkerSlotState,
};

const AUDIT_GATE_ENV: &str = "WINWINCODE_LIVE_PROVIDER_DELIVERY_AUDIT_GATE";
const EVIDENCE_FILE_ENV: &str = "WINWINCODE_LIVE_PROVIDER_DELIVERY_EVIDENCE_FILE";
const SECRET_FILE_ENV: &str = "WINWINCODE_LIVE_PROVIDER_SECRET_FILE";
const PRIVATE_INPUT_FILE_ENV: &str = "WINWINCODE_LIVE_PROVIDER_PRIVATE_INPUT_FILE";
const EVIDENCE_SCHEMA: &str = "winwincode.live-provider-delivery-evidence.v1";
const MAX_EVIDENCE_BYTES: u64 = 64 * 1024;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const SCAN_BUFFER_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AuditErrorKind {
    Evidence,
    DurableFacts,
    RestrictedBytes,
}

#[derive(Debug)]
struct AuditError {
    kind: AuditErrorKind,
}

impl AuditError {
    const fn new(kind: AuditErrorKind) -> Self {
        Self { kind }
    }

    const fn evidence() -> Self {
        Self::new(AuditErrorKind::Evidence)
    }

    const fn durable() -> Self {
        Self::new(AuditErrorKind::DurableFacts)
    }

    const fn restricted() -> Self {
        Self::new(AuditErrorKind::RestrictedBytes)
    }
}

impl fmt::Display for AuditError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("live Provider Delivery audit failed")
    }
}

impl Error for AuditError {}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LiveProviderDeliveryEvidence {
    schema: String,
    data_directory: PathBuf,
    repository_root: PathBuf,
    delivery_id: DeliveryId,
    provider_runs: Vec<ProviderRunEvidence>,
    pool_config: PoolConfigEvidence,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProviderRunEvidence {
    execution_job_id: ExecutionJobId,
    model_exchange_id: ModelExchangeId,
    provider_request_id: RequestId,
    provider_usage_id: String,
    worker_session_id: WorkerSessionId,
    lease_id: LeaseId,
    model_admission_terminal_request_id: RequestId,
    lease_terminal_request_id: RequestId,
    budget_period_id: String,
    expected_usage: ExpectedProviderUsage,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FinalAckEvidence {
    schema: String,
    model_exchange_id: ModelExchangeId,
    acknowledged_sequence: u64,
}

struct ExpectedAdmission {
    authority: FrozenModelRouteAuthority,
    budget_period_id: String,
    tokens: u64,
    cost_micros: u64,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PoolConfigEvidence {
    max_routes: usize,
    max_active_per_route: usize,
    max_waiting_per_route: usize,
    max_exchange_records_per_route: usize,
    max_buffered_frames_per_stream: usize,
    max_buffered_bytes_per_stream: usize,
    resume_buffered_frames_per_stream: usize,
    resume_buffered_bytes_per_stream: usize,
}

impl From<PoolConfigEvidence> for ModelRequestPoolConfig {
    fn from(value: PoolConfigEvidence) -> Self {
        Self {
            max_routes: value.max_routes,
            max_active_per_route: value.max_active_per_route,
            max_waiting_per_route: value.max_waiting_per_route,
            max_exchange_records_per_route: value.max_exchange_records_per_route,
            max_buffered_frames_per_stream: value.max_buffered_frames_per_stream,
            max_buffered_bytes_per_stream: value.max_buffered_bytes_per_stream,
            resume_buffered_frames_per_stream: value.resume_buffered_frames_per_stream,
            resume_buffered_bytes_per_stream: value.resume_buffered_bytes_per_stream,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ExpectedProviderUsage {
    input_tokens: u64,
    cached_input_tokens: u64,
    cache_write_input_tokens: u64,
    output_tokens: u64,
    reasoning_output_tokens: u64,
    cost_micros: u64,
}

impl ExpectedProviderUsage {
    fn total_tokens(self) -> Result<u64, AuditError> {
        let total = [self.input_tokens, self.output_tokens]
            .into_iter()
            .try_fold(0_u64, u64::checked_add)
            .ok_or_else(AuditError::evidence)?;
        (total <= MAX_SAFE_INTEGER)
            .then_some(total)
            .ok_or_else(AuditError::evidence)
    }
}

struct AuditClock;

impl ModelAdmissionClock for AuditClock {
    fn unix_minute(&self) -> Result<u64, ModelAdmissionClockError> {
        Ok(0)
    }
}

#[test]
fn restricted_byte_scan_catches_a_marker_split_across_read_chunks() {
    let root = std::env::temp_dir().join(format!(
        "winwincode-live-provider-audit-scan-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("logs")).expect("create audit scan fixture");
    let marker = b"restricted-live-provider-marker".to_vec();
    let mut bytes = vec![b'x'; SCAN_BUFFER_BYTES - marker.len() / 2];
    bytes.extend_from_slice(&marker);
    fs::write(root.join("logs/provider.trace"), bytes).expect("write split audit marker");
    assert_eq!(
        scan_restricted_bytes(&root, std::slice::from_ref(&marker))
            .expect_err("split restricted marker must be found")
            .kind,
        AuditErrorKind::RestrictedBytes
    );
    fs::write(root.join("logs/provider.trace"), b"secret-free trace")
        .expect("replace audit fixture");
    scan_restricted_bytes(&root, &[marker]).expect("secret-free files pass the scanner");
    fs::remove_dir_all(root).expect("remove audit scan fixture");
}

#[test]
fn provider_total_does_not_double_count_cached_or_reasoning_subtotals() {
    let usage = ExpectedProviderUsage {
        input_tokens: 10,
        cached_input_tokens: 4,
        cache_write_input_tokens: 3,
        output_tokens: 7,
        reasoning_output_tokens: 5,
        cost_micros: 2,
    };
    assert_eq!(usage.total_tokens().expect("bounded total"), 17);
}

#[test]
fn restricted_markers_include_the_payload_without_terminal_line_endings() {
    let markers = restricted_markers(b"private-input-marker\r\n".to_vec());
    assert_eq!(
        markers,
        vec![
            b"private-input-marker\r\n".to_vec(),
            b"private-input-marker".to_vec()
        ]
    );
}

#[test]
#[ignore = "requires explicit mode-0600 live Provider evidence and restricted-byte files"]
fn live_provider_delivery_usage_secrets_and_resources_reconcile_exactly() {
    assert_eq!(
        std::env::var(AUDIT_GATE_ENV).as_deref(),
        Ok("1"),
        "set the explicit live Provider Delivery audit gate to 1"
    );
    let evidence_path = required_private_file(EVIDENCE_FILE_ENV);
    let secret_path = required_private_file(SECRET_FILE_ENV);
    let private_input_path = required_private_file(PRIVATE_INPUT_FILE_ENV);
    let evidence = load_evidence(&evidence_path).expect("load secret-free live evidence");
    let restricted = [secret_path, private_input_path]
        .into_iter()
        .flat_map(|path| restricted_markers(fs::read(path).expect("read restricted input file")))
        .collect::<Vec<_>>();
    assert!(restricted.iter().all(|value| !value.is_empty()));

    audit_delivery(&evidence).expect("audit live Provider Delivery durable facts");
    scan_restricted_bytes(&evidence.data_directory, &restricted)
        .expect("scan durable Provider Delivery files");

    let mut changed = evidence.clone();
    let changed_request_id = changed.provider_runs[1].provider_request_id.clone();
    let run = changed
        .provider_runs
        .first_mut()
        .expect("validated Provider run evidence");
    run.provider_request_id = changed_request_id;
    assert_eq!(
        audit_delivery(&changed)
            .expect_err("changed evidence must fail closed")
            .kind,
        AuditErrorKind::DurableFacts
    );
}

fn restricted_markers(value: Vec<u8>) -> Vec<Vec<u8>> {
    let trimmed_length = value
        .iter()
        .rposition(|byte| !matches!(*byte, b'\r' | b'\n'))
        .map_or(0, |position| position + 1);
    if trimmed_length == 0 || trimmed_length == value.len() {
        vec![value]
    } else {
        let trimmed = value[..trimmed_length].to_vec();
        vec![value, trimmed]
    }
}

fn required_private_file(environment: &str) -> PathBuf {
    let path = PathBuf::from(std::env::var_os(environment).expect("configured private file"));
    let metadata = fs::symlink_metadata(&path).expect("private file metadata");
    assert!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "configured private path must be a regular file"
    );
    #[cfg(unix)]
    assert_eq!(
        metadata.permissions().mode() & 0o777,
        0o600,
        "private file permissions must be 0600"
    );
    path
}

fn load_evidence(path: &Path) -> Result<LiveProviderDeliveryEvidence, AuditError> {
    let metadata = fs::metadata(path).map_err(|_| AuditError::evidence())?;
    if metadata.len() == 0 || metadata.len() > MAX_EVIDENCE_BYTES {
        return Err(AuditError::evidence());
    }
    let bytes = fs::read(path).map_err(|_| AuditError::evidence())?;
    let evidence: LiveProviderDeliveryEvidence =
        serde_json::from_slice(&bytes).map_err(|_| AuditError::evidence())?;
    if evidence.schema != EVIDENCE_SCHEMA
        || evidence.provider_runs.len() != 4
        || !evidence.data_directory.is_dir()
        || !evidence.repository_root.is_dir()
        || fs::canonicalize(&evidence.data_directory).map_err(|_| AuditError::evidence())?
            == fs::canonicalize(&evidence.repository_root).map_err(|_| AuditError::evidence())?
    {
        return Err(AuditError::evidence());
    }
    let mut identities = BTreeSet::new();
    for run in &evidence.provider_runs {
        if run.budget_period_id.is_empty()
            || run.provider_usage_id.is_empty()
            || run.expected_usage.cached_input_tokens > run.expected_usage.input_tokens
            || run.expected_usage.cache_write_input_tokens > run.expected_usage.input_tokens
            || run.expected_usage.reasoning_output_tokens > run.expected_usage.output_tokens
            || run.expected_usage.cost_micros > MAX_SAFE_INTEGER
        {
            return Err(AuditError::evidence());
        }
        for identity in [
            format!("exchange:{}", run.model_exchange_id.0),
            format!("provider-request:{}", run.provider_request_id.0),
            format!("provider-usage:{}", run.provider_usage_id),
            format!("job:{}", run.execution_job_id.0),
            format!("worker-session:{}", run.worker_session_id.0),
            format!("lease:{}", run.lease_id.0),
            format!(
                "model-admission-terminal:{}",
                run.model_admission_terminal_request_id.0
            ),
            format!("lease-terminal:{}", run.lease_terminal_request_id.0),
        ] {
            if !identities.insert(identity) {
                return Err(AuditError::evidence());
            }
        }
        run.expected_usage.total_tokens()?;
    }
    Ok(evidence)
}

fn audit_delivery(evidence: &LiveProviderDeliveryEvidence) -> Result<(), AuditError> {
    let mut pool_authority = None;
    let mut admissions = Vec::new();
    for run in &evidence.provider_runs {
        let (route_authority, current_pool) = audit_exchange(evidence, run)?;
        let settled_at = audit_gateway_terminal(evidence, run)?;
        audit_usage(evidence, run, &route_authority, &settled_at)?;
        add_expected_admission(&mut admissions, run, &route_authority)?;
        audit_slot_and_lease(evidence, run)?;
        pool_authority = Some(current_pool);
    }
    audit_usage_catalog(evidence)?;
    audit_admissions(evidence, &admissions)?;
    audit_pool(evidence, &pool_authority.ok_or_else(AuditError::evidence)?)?;
    Ok(())
}

fn audit_exchange(
    evidence: &LiveProviderDeliveryEvidence,
    run: &ProviderRunEvidence,
) -> Result<(FrozenModelRouteAuthority, Vec<u8>), AuditError> {
    let mut storage =
        SqliteStorage::open(&evidence.data_directory).map_err(|_| AuditError::durable())?;
    let (authority, pool) = {
        let exchange_store = storage
            .provider_exchange_store()
            .map_err(|_| AuditError::durable())?;
        let snapshot = exchange_store
            .load(&run.model_exchange_id)
            .map_err(|_| AuditError::durable())?
            .ok_or_else(AuditError::durable)?;
        if snapshot.state != ProviderExchangeState::Terminal
            || snapshot.model_exchange_id != run.model_exchange_id
            || snapshot.request_id != run.provider_request_id
            || snapshot.terminal_receipt_json().is_none()
        {
            return Err(AuditError::durable());
        }
        let authority = FrozenModelRouteAuthority::from_durable_json(
            snapshot
                .route_authority_json()
                .ok_or_else(AuditError::durable)?,
        )
        .map_err(|_| AuditError::durable())?;
        let final_ack = exchange_store
            .load_final_ack(&run.model_exchange_id)
            .map_err(|_| AuditError::durable())?
            .ok_or_else(AuditError::durable)?;
        let final_ack_receipt: FinalAckEvidence =
            serde_json::from_slice(final_ack.receipt_json()).map_err(|_| AuditError::durable())?;
        if final_ack.model_exchange_id != run.model_exchange_id
            || final_ack_receipt.schema != "winwincode.provider-exchange-final-ack.v1"
            || final_ack_receipt.model_exchange_id != run.model_exchange_id
            || i64::try_from(final_ack_receipt.acknowledged_sequence)
                .map_err(|_| AuditError::durable())?
                != final_ack.ack_sequence
            || serde_json::to_vec(&final_ack_receipt).map_err(|_| AuditError::durable())?
                != final_ack.receipt_json()
        {
            return Err(AuditError::durable());
        }
        let pool = exchange_store
            .load_pool_authority()
            .map_err(|_| AuditError::durable())?
            .ok_or_else(AuditError::durable)?
            .state_json()
            .to_vec();
        (authority, pool)
    };
    Box::new(storage)
        .close()
        .map_err(|_| AuditError::durable())?;
    Ok((authority, pool))
}

fn audit_gateway_terminal(
    evidence: &LiveProviderDeliveryEvidence,
    run: &ProviderRunEvidence,
) -> Result<Instant, AuditError> {
    let exchange = DurableModelExchangeAuthority::open(&evidence.data_directory)
        .map_err(|_| AuditError::durable())?;
    let progress = ProviderGatewayTerminalProgressPort::load(&exchange, &run.model_exchange_id)
        .map_err(|_| AuditError::durable())?
        .ok_or_else(AuditError::durable)?;
    let terminal = progress.terminal.ok_or_else(AuditError::durable)?;
    if progress.stage != ProviderGatewayTerminalProgressStage::SettlementSettled
        || terminal.model_exchange_id != run.model_exchange_id
        || terminal.outcome != ProviderGatewayTerminalOutcome::Succeeded
        || terminal.admission.request_id != run.model_admission_terminal_request_id
        || terminal.admission.model_exchange_id != run.model_exchange_id
        || terminal.admission.outcome != ModelReservationTerminalOutcome::Completed
        || terminal.admission.actual_tokens != run.expected_usage.total_tokens()?
        || terminal.admission.actual_cost_micros != run.expected_usage.cost_micros
        || !terminal.idempotent_replay
    {
        return Err(AuditError::durable());
    }
    let settled_at = terminal.settled_at;
    exchange.close().map_err(|_| AuditError::durable())?;
    Ok(settled_at)
}

fn audit_usage(
    evidence: &LiveProviderDeliveryEvidence,
    run: &ProviderRunEvidence,
    authority: &FrozenModelRouteAuthority,
    settled_at: &Instant,
) -> Result<(), AuditError> {
    let mut storage =
        SqliteStorage::open(&evidence.data_directory).map_err(|_| AuditError::durable())?;
    let usage = ModelRetryUsageService::new(&mut storage)
        .usage_source(&run.provider_usage_id)
        .map_err(|_| AuditError::durable())?
        .ok_or_else(AuditError::durable)?;
    let expected = run.expected_usage;
    if usage.request_id != run.provider_request_id
        || usage.model_exchange_id != run.model_exchange_id
        || usage.attribution.delivery_id.as_ref() != Some(&evidence.delivery_id)
        || usage.usage.provider_usage_id != run.provider_usage_id
        || usage.usage.provider_id != authority.route().provider_id
        || usage.usage.model_id != authority.route().model_id
        || usage.usage.input_tokens != expected.input_tokens
        || usage.usage.cached_input_tokens != expected.cached_input_tokens
        || usage.usage.cache_write_input_tokens != expected.cache_write_input_tokens
        || usage.usage.output_tokens != expected.output_tokens
        || usage.usage.reasoning_output_tokens != expected.reasoning_output_tokens
        || usage.usage.total_tokens != expected.total_tokens()?
        || usage.usage.cost_micros != expected.cost_micros
        || usage.route_authority_fingerprint != authority.fingerprint()
        || &usage.settled_at != settled_at
    {
        return Err(AuditError::durable());
    }
    let repository_scope = RepositoryScope {
        kind: RepositoryScopeKind::Repository,
        organization_id: usage.attribution.organization_id.clone(),
        workspace_id: usage.attribution.workspace_id.clone(),
        project_id: usage.attribution.project_id.clone(),
        repository_id: usage.attribution.repository_id.clone(),
    };
    let delivery = load_delivery_authority_seal(&storage, &repository_scope, &evidence.delivery_id)
        .map_err(|_| AuditError::durable())?;
    if delivery.delivery.snapshot().status != DeliveryStatus::Delivered {
        return Err(AuditError::durable());
    }
    audit_delivery_status_history(&storage, evidence, &delivery.delivery)?;
    let enterprise_source = EnterpriseUsageSource::Provider {
        provider_usage_id: usage.usage.provider_usage_id.clone(),
        source_sequence: usage.sequence,
        source_digest: usage.source_digest.clone(),
        model_exchange_id: usage.model_exchange_id.clone(),
        request_id: usage.request_id.clone(),
        attempt: usage.usage.attempt,
        route_authority_fingerprint: usage.route_authority_fingerprint.clone(),
    };
    let enterprise = storage
        .enterprise_usage_ledger()
        .map_err(|_| AuditError::durable())?
        .load_source(&enterprise_source)
        .map_err(|_| AuditError::durable())?
        .ok_or_else(AuditError::durable)?;
    let EnterpriseUsageMeasure::Provider {
        input_tokens,
        cached_input_tokens,
        cache_write_input_tokens,
        output_tokens,
        reasoning_output_tokens,
        total_tokens,
        cost_micros,
    } = enterprise.fact.measure
    else {
        return Err(AuditError::durable());
    };
    if enterprise.fact.source != enterprise_source
        || enterprise.fact.attribution.organization_id != usage.attribution.organization_id
        || enterprise.fact.attribution.workspace_id != usage.attribution.workspace_id
        || enterprise.fact.attribution.project_id != usage.attribution.project_id
        || enterprise.fact.attribution.repository_id != usage.attribution.repository_id
        || enterprise.fact.attribution.delivery_id.as_ref() != Some(&evidence.delivery_id)
        || enterprise.fact.attribution.product_session_id.as_ref()
            != Some(&usage.attribution.product_session_id)
        || enterprise.fact.attribution.user_id != usage.attribution.user_id
        || input_tokens != expected.input_tokens
        || cached_input_tokens != expected.cached_input_tokens
        || cache_write_input_tokens != expected.cache_write_input_tokens
        || output_tokens != expected.output_tokens
        || reasoning_output_tokens != expected.reasoning_output_tokens
        || total_tokens != expected.total_tokens()?
        || cost_micros != expected.cost_micros
        || &enterprise.fact.settled_at != settled_at
    {
        return Err(AuditError::durable());
    }
    Box::new(storage).close().map_err(|_| AuditError::durable())
}

fn add_expected_admission(
    admissions: &mut Vec<ExpectedAdmission>,
    run: &ProviderRunEvidence,
    authority: &FrozenModelRouteAuthority,
) -> Result<(), AuditError> {
    let tokens = run.expected_usage.total_tokens()?;
    if let Some(expected) = admissions.iter_mut().find(|expected| {
        expected.authority.fingerprint() == authority.fingerprint()
            && expected.budget_period_id == run.budget_period_id
    }) {
        expected.tokens = expected
            .tokens
            .checked_add(tokens)
            .filter(|total| *total <= MAX_SAFE_INTEGER)
            .ok_or_else(AuditError::evidence)?;
        expected.cost_micros = expected
            .cost_micros
            .checked_add(run.expected_usage.cost_micros)
            .filter(|total| *total <= MAX_SAFE_INTEGER)
            .ok_or_else(AuditError::evidence)?;
    } else {
        admissions.push(ExpectedAdmission {
            authority: authority.clone(),
            budget_period_id: run.budget_period_id.clone(),
            tokens,
            cost_micros: run.expected_usage.cost_micros,
        });
    }
    Ok(())
}

fn audit_admissions(
    evidence: &LiveProviderDeliveryEvidence,
    expected: &[ExpectedAdmission],
) -> Result<(), AuditError> {
    let mut storage =
        SqliteStorage::open(&evidence.data_directory).map_err(|_| AuditError::durable())?;
    for expected in expected {
        let admission = ModelAdmissionService::new(&mut storage, &AuditClock)
            .snapshot(&expected.authority, &expected.budget_period_id)
            .map_err(|_| AuditError::durable())?;
        if admission.active_reservations != 0
            || admission.budget_reserved_tokens != 0
            || admission.budget_reserved_cost_micros != 0
            || admission.budget_settled_tokens != expected.tokens
            || admission.budget_settled_cost_micros != expected.cost_micros
        {
            return Err(AuditError::durable());
        }
    }
    Box::new(storage).close().map_err(|_| AuditError::durable())
}

fn audit_usage_catalog(evidence: &LiveProviderDeliveryEvidence) -> Result<(), AuditError> {
    let expected = evidence
        .provider_runs
        .iter()
        .map(|run| {
            (
                run.provider_usage_id.clone(),
                run.model_exchange_id.0.clone(),
                run.provider_request_id.0.clone(),
            )
        })
        .collect::<BTreeSet<_>>();
    let mut storage =
        SqliteStorage::open(&evidence.data_directory).map_err(|_| AuditError::durable())?;
    let actual = {
        let service = ModelRetryUsageService::new(&mut storage);
        let filter = ModelUsageFilter {
            delivery_id: Some(evidence.delivery_id.clone()),
            ..ModelUsageFilter::default()
        };
        let mut cursor = None;
        let mut actual = BTreeSet::new();
        let mut product_sessions = BTreeSet::new();
        loop {
            let page = service
                .scan_usage_sources(&filter, cursor.as_ref(), 200)
                .map_err(|_| AuditError::durable())?;
            for entry in page.entries {
                if !product_sessions.insert(entry.attribution.product_session_id.0.clone()) {
                    return Err(AuditError::durable());
                }
                if !actual.insert((
                    entry.usage.provider_usage_id,
                    entry.model_exchange_id.0,
                    entry.request_id.0,
                )) {
                    return Err(AuditError::durable());
                }
            }
            let Some(next) = page.next else {
                break;
            };
            cursor = Some(next);
        }
        if product_sessions.len() != evidence.provider_runs.len() {
            return Err(AuditError::durable());
        }
        actual
    };
    if actual != expected {
        return Err(AuditError::durable());
    }
    Box::new(storage).close().map_err(|_| AuditError::durable())
}

fn audit_delivery_status_history(
    storage: &dyn ProductStateStorage,
    evidence: &LiveProviderDeliveryEvidence,
    current: &winwincode_delivery::domain::Delivery,
) -> Result<(), AuditError> {
    let key = AggregateJournalKey::new("delivery", &evidence.delivery_id.0)
        .map_err(|_| AuditError::durable())?;
    let loaded = storage
        .load_journal(&key)
        .map_err(|_| AuditError::durable())?
        .ok_or_else(AuditError::durable)?;
    let journal = LoadedDeliveryJournal {
        manifest: loaded.manifest,
        records: loaded
            .records
            .into_iter()
            .map(|record| JournalRecordBytes {
                sequence: record.sequence,
                state: JournalEntryState::Published,
                digest: record.digest,
                bytes: record.payload,
            })
            .collect(),
    };
    let verified = DeliveryJournalCodec::verify(&evidence.delivery_id, journal)
        .map_err(|_| AuditError::durable())?;
    if &verified.snapshot != current {
        return Err(AuditError::durable());
    }
    let verdict_index = verified
        .records
        .iter()
        .rposition(|record| {
            record.operation == DeliveryMutationOperation::VerdictSubmitted
                && record.snapshot.snapshot().status == DeliveryStatus::ReadyToDeliver
                && record.snapshot.snapshot().verdict.is_some()
        })
        .ok_or_else(AuditError::durable)?;
    let review_resolved = verified.records[verdict_index + 1..].iter().any(|record| {
        record.operation == DeliveryMutationOperation::AttentionResolved
            && record.snapshot.snapshot().status == DeliveryStatus::Delivered
            && record.snapshot.snapshot().verdict.is_some()
    });
    review_resolved
        .then_some(())
        .ok_or_else(AuditError::durable)
}

fn audit_pool(
    evidence: &LiveProviderDeliveryEvidence,
    pool_authority: &[u8],
) -> Result<(), AuditError> {
    let mut pool =
        ModelRequestPool::new(evidence.pool_config.into()).map_err(|_| AuditError::durable())?;
    pool.restore_authority(pool_authority)
        .map_err(|_| AuditError::durable())?;
    if !pool.is_empty() {
        return Err(AuditError::durable());
    }
    Ok(())
}

fn audit_slot_and_lease(
    evidence: &LiveProviderDeliveryEvidence,
    run: &ProviderRunEvidence,
) -> Result<(), AuditError> {
    let mut storage =
        SqliteStorage::open(&evidence.data_directory).map_err(|_| AuditError::durable())?;
    let slot = storage
        .worker_session_slots()
        .map_err(|_| AuditError::durable())?
        .load(&run.worker_session_id)
        .map_err(|_| AuditError::durable())?
        .ok_or_else(AuditError::durable)?;
    if slot.authority.worker_session_id != run.worker_session_id
        || slot.authority.job_id != run.execution_job_id
        || slot.authority.lease_id != run.lease_id
        || slot.state != WorkerSlotState::Completed
    {
        return Err(AuditError::durable());
    }
    let capacity = storage
        .worker_session_slots()
        .map_err(|_| AuditError::durable())?
        .capacity(
            &slot.authority.worker_id,
            &slot.authority.worker_instance_id,
        )
        .map_err(|_| AuditError::durable())?;
    if capacity.running_slots != 0
        || capacity.reserved
            != (WorkerSlotResources {
                memory_bytes: 0,
                disk_bytes: 0,
                process_slots: 0,
            })
    {
        return Err(AuditError::durable());
    }
    let lease = storage
        .execution_registry()
        .map_err(|_| AuditError::durable())?
        .load_lease(&run.execution_job_id)
        .map_err(|_| AuditError::durable())?
        .ok_or_else(AuditError::durable)?;
    if lease.lease_id != run.lease_id
        || lease.worker_id != slot.authority.worker_id
        || lease.worker_instance_id != slot.authority.worker_instance_id
        || lease.attempt != slot.authority.attempt
        || lease.fencing_token != slot.authority.fencing_token
    {
        return Err(AuditError::durable());
    }
    let terminal_at = slot.terminal_at.clone().ok_or_else(AuditError::durable)?;
    let exact = ExecutionLeaseTerminalRequest {
        job_id: run.execution_job_id.clone(),
        lease_id: run.lease_id.clone(),
        worker_id: slot.authority.worker_id,
        worker_instance_id: slot.authority.worker_instance_id,
        attempt: slot.authority.attempt,
        fencing_token: slot.authority.fencing_token,
        outcome: ExecutionLeaseTerminalOutcome::Completed,
        terminal_at,
        request_id: run.lease_terminal_request_id.clone(),
    };
    if storage
        .execution_registry()
        .map_err(|_| AuditError::durable())?
        .finish_execution_lease(&exact)
        .map_err(|_| AuditError::durable())?
    {
        return Err(AuditError::durable());
    }
    let mut changed = exact;
    changed.outcome = ExecutionLeaseTerminalOutcome::Failed;
    if storage
        .execution_registry()
        .map_err(|_| AuditError::durable())?
        .finish_execution_lease(&changed)
        .is_ok()
    {
        return Err(AuditError::durable());
    }
    Box::new(storage).close().map_err(|_| AuditError::durable())
}

fn scan_restricted_bytes(root: &Path, restricted: &[Vec<u8>]) -> Result<(), AuditError> {
    if restricted.iter().any(Vec::is_empty) {
        return Err(AuditError::evidence());
    }
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let metadata = fs::symlink_metadata(&path).map_err(|_| AuditError::restricted())?;
        if metadata.file_type().is_symlink() {
            return Err(AuditError::restricted());
        }
        if metadata.is_dir() {
            for entry in fs::read_dir(&path).map_err(|_| AuditError::restricted())? {
                pending.push(entry.map_err(|_| AuditError::restricted())?.path());
            }
        } else if metadata.is_file() {
            for needle in restricted {
                if file_contains(&path, needle).map_err(|_| AuditError::restricted())? {
                    return Err(AuditError::restricted());
                }
            }
        }
    }
    Ok(())
}

fn file_contains(path: &Path, needle: &[u8]) -> io::Result<bool> {
    let mut file = fs::File::open(path)?;
    let overlap = needle.len().saturating_sub(1);
    let mut carried = Vec::with_capacity(overlap);
    let mut buffer = vec![0_u8; SCAN_BUFFER_BYTES];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            return Ok(false);
        }
        let mut window = Vec::with_capacity(carried.len() + read);
        window.extend_from_slice(&carried);
        window.extend_from_slice(&buffer[..read]);
        if window.windows(needle.len()).any(|value| value == needle) {
            return Ok(true);
        }
        carried.clear();
        carried.extend_from_slice(&window[window.len().saturating_sub(overlap)..]);
    }
}
