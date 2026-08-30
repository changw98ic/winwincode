// SPDX-License-Identifier: Apache-2.0

//! Durable `StrongFlow` stage-product projection over the canonical runtime stream.
//!
//! The projector accepts only trusted Codex lifecycle facts. It prepares the
//! role-specific semantic product, assigns a deterministic event identity, and
//! gives the complete generated frame to the existing runtime replay state
//! machine before the caller may expose it or complete the turn.

use std::{collections::HashMap, fmt};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use winwincode_domain::{
    ExecutionEventId, ExecutionMessageId, ExecutionSequence, Instant, SchemaVersion,
    SessionIdentity, Sha256Digest, WorkerSessionId,
};
use winwincode_execution_port::{
    generated::{
        EncodedPayload, ExecutionEventCategory, ExecutionEventRecord, ExecutionJob,
        ExecutionLeaseStamp, RuntimeEventMessage, RuntimeEventMessageKind,
    },
    replay::{ReplayAuthority, ReplayDecision, ReplaySnapshot, ReplayStore},
    runtime_replay::{RuntimeReplayError, RuntimeReplayIdentity, RuntimeReplayResponder},
};

use crate::stage_product::{
    PreparedStageProduct, StageProductError, VerificationEvidenceKind, VerificationEvidenceStatus,
    prepare_planner_solution_activity, prepare_verification_command_evidence,
    prepare_verification_policy_attestation, prepare_verification_result_activity,
    stage_product_job_digest,
};

const MAX_SOURCE_ID_BYTES: usize = 4_096;
const JSON_CONTENT_TYPE: &str = "application/json";
const VERIFICATION_POLICY_PROTOCOL: &str = "winwincode.verification-session-policy.v1";

/// Lease-bound identity and timestamps supplied by the production adapter.
#[derive(Clone, Copy, Debug)]
pub struct StageRuntimeContext<'identity> {
    pub lease: &'identity ExecutionLeaseStamp,
    pub worker_session_id: &'identity WorkerSessionId,
    pub session_identity: &'identity SessionIdentity,
    pub occurred_at: &'identity Instant,
    pub sent_at: &'identity Instant,
}

/// Trusted fields from one completed Codex command.
#[derive(Clone, Copy, Debug)]
pub struct StageCommandEnd<'source> {
    pub command: &'source [String],
    pub turn_id: &'source str,
    pub call_id: &'source str,
    pub status: VerificationEvidenceStatus,
    pub exit_code: i64,
}

/// Trusted fields from one completed Codex turn.
#[derive(Clone, Copy, Debug)]
pub struct StageTurnCompletion<'source> {
    pub turn_id: &'source str,
    pub final_message: Option<&'source str>,
    pub failed: bool,
}

/// Durable projection result for one trusted Codex source fact.
#[derive(Clone, Debug, PartialEq)]
pub enum StageRuntimeRetention {
    Ready {
        message: Box<RuntimeEventMessage>,
        duplicate: bool,
    },
    Gap {
        highest_sequence: u64,
        replay_from_sequence: u64,
    },
    Conflict {
        highest_sequence: u64,
    },
}

/// Fail-closed stage runtime projection error.
#[derive(Debug)]
pub enum StageRuntimeProjectionError<AuthorityError, StoreError> {
    Product(StageProductError),
    Store(StoreError),
    Replay(RuntimeReplayError<AuthorityError, StoreError>),
    InvalidSource,
    InvalidEvidence,
    CorruptOriginal,
}

impl<AuthorityError: fmt::Debug, StoreError: fmt::Debug> fmt::Display
    for StageRuntimeProjectionError<AuthorityError, StoreError>
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Product(_) => "stage product is invalid",
            Self::Store(_) => "stage runtime replay store failed",
            Self::Replay(_) => "stage runtime replay rejected the event",
            Self::InvalidSource => "Codex stage-product source identity is invalid",
            Self::InvalidEvidence => "verification result does not cite retained direct evidence",
            Self::CorruptOriginal => "retained stage runtime frame is corrupt",
        })
    }
}

impl<AuthorityError: fmt::Debug, StoreError: fmt::Debug> std::error::Error
    for StageRuntimeProjectionError<AuthorityError, StoreError>
{
}

/// Stateless coordinator over the existing durable runtime replay authority.
#[derive(Debug, Default)]
pub struct StageRuntimeProjector {
    responder: RuntimeReplayResponder,
}

#[derive(Clone, Copy)]
struct Projection<'product> {
    job: &'product ExecutionJob,
    source: &'product str,
    product: &'product PreparedStageProduct,
}

impl StageRuntimeProjector {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            responder: RuntimeReplayResponder::new(),
        }
    }

    /// Retains the verification read-only policy before direct evidence.
    ///
    /// Non-verification roles produce no stage product at turn start.
    ///
    /// # Errors
    ///
    /// Rejects malformed source identity, stale stage input, replay conflicts,
    /// or durable store failures.
    pub fn retain_turn_started<S, A>(
        &self,
        store: &mut S,
        authority: &A,
        context: StageRuntimeContext<'_>,
        job: &ExecutionJob,
        turn_id: &str,
    ) -> Result<Option<StageRuntimeRetention>, StageRuntimeProjectionError<A::Error, S::Error>>
    where
        S: ReplayStore,
        A: ReplayAuthority<Context = RuntimeReplayIdentity>,
    {
        if !verification_role(&job.execution_profile) {
            return Ok(None);
        }
        validate_source_id(turn_id)?;
        let candidate_ref = job
            .stage_input
            .as_ref()
            .and_then(|input| input.candidate_ref.as_deref())
            .unwrap_or_default();
        let product = prepare_verification_policy_attestation(job, candidate_ref)
            .map_err(StageRuntimeProjectionError::Product)?;
        self.retain_product(
            store,
            authority,
            context,
            job,
            &source_key("policy", turn_id, None),
            &product,
        )
        .map(Some)
    }

    /// Retains one direct Command or Test outcome after the policy event.
    ///
    /// # Errors
    ///
    /// Rejects an unknown source, missing policy predecessor, malformed direct
    /// outcome, changed duplicate, replay conflict, or durable store failure.
    pub fn retain_exec_command_end<S, A>(
        &self,
        store: &mut S,
        authority: &A,
        context: StageRuntimeContext<'_>,
        job: &ExecutionJob,
        command_end: StageCommandEnd<'_>,
    ) -> Result<Option<StageRuntimeRetention>, StageRuntimeProjectionError<A::Error, S::Error>>
    where
        S: ReplayStore,
        A: ReplayAuthority<Context = RuntimeReplayIdentity>,
    {
        if !verification_role(&job.execution_profile) {
            return Ok(None);
        }
        validate_source_id(command_end.turn_id)?;
        validate_source_id(command_end.call_id)?;
        let identity = replay_identity(context);
        let snapshot = load_snapshot(store, &identity)?;
        let policy_id = projected_event_id(job, &source_key("policy", command_end.turn_id, None))
            .map_err(StageRuntimeProjectionError::Product)?;
        if !snapshot_has_policy(&snapshot, &policy_id) {
            return Err(StageRuntimeProjectionError::InvalidEvidence);
        }
        let product = prepare_verification_command_evidence(
            job,
            classify_evidence(command_end.command),
            command_end.status,
            command_end.exit_code,
            command_end.call_id,
        )
        .map_err(StageRuntimeProjectionError::Product)?;
        self.retain_product_with_snapshot(
            store,
            authority,
            context,
            Projection {
                job,
                source: &source_key("evidence", command_end.turn_id, Some(command_end.call_id)),
                product: &product,
            },
            &snapshot,
        )
        .map(Some)
    }

    /// Retains the strict Planner or verification final Activity before turn
    /// completion is allowed to become terminal.
    ///
    /// A failed Codex turn produces no semantic result. Successful relevant
    /// turns require a canonical final message; verification references are
    /// resolved only against earlier direct evidence in this runtime stream.
    ///
    /// # Errors
    ///
    /// Rejects missing or malformed final JSON, stale stage facts, result
    /// references without an earlier direct event, changed duplicates, replay
    /// conflicts, or durable store failures.
    pub fn retain_turn_completed<S, A>(
        &self,
        store: &mut S,
        authority: &A,
        context: StageRuntimeContext<'_>,
        job: &ExecutionJob,
        completion: StageTurnCompletion<'_>,
    ) -> Result<Option<StageRuntimeRetention>, StageRuntimeProjectionError<A::Error, S::Error>>
    where
        S: ReplayStore,
        A: ReplayAuthority<Context = RuntimeReplayIdentity>,
    {
        if completion.failed {
            return Ok(None);
        }
        validate_source_id(completion.turn_id)?;
        let Some(final_message) = completion.final_message else {
            if job.execution_profile == "planner" || verification_role(&job.execution_profile) {
                return Err(StageRuntimeProjectionError::InvalidSource);
            }
            return Ok(None);
        };
        let identity = replay_identity(context);
        let snapshot = load_snapshot(store, &identity)?;
        let product = match job.execution_profile.as_str() {
            "planner" => prepare_planner_solution_activity(job, final_message.as_bytes())
                .map_err(StageRuntimeProjectionError::Product)?,
            role if verification_role(role) => {
                let policy_id =
                    projected_event_id(job, &source_key("policy", completion.turn_id, None))
                        .map_err(StageRuntimeProjectionError::Product)?;
                if !snapshot_has_policy(&snapshot, &policy_id) {
                    return Err(StageRuntimeProjectionError::InvalidEvidence);
                }
                let bound = bind_verification_evidence(final_message.as_bytes(), &snapshot)?;
                prepare_verification_result_activity(job, &bound)
                    .map_err(StageRuntimeProjectionError::Product)?
            }
            _ => return Ok(None),
        };
        self.retain_product_with_snapshot(
            store,
            authority,
            context,
            Projection {
                job,
                source: &source_key("result", completion.turn_id, None),
                product: &product,
            },
            &snapshot,
        )
        .map(Some)
    }

    fn retain_product<S, A>(
        &self,
        store: &mut S,
        authority: &A,
        context: StageRuntimeContext<'_>,
        job: &ExecutionJob,
        source: &str,
        product: &PreparedStageProduct,
    ) -> Result<StageRuntimeRetention, StageRuntimeProjectionError<A::Error, S::Error>>
    where
        S: ReplayStore,
        A: ReplayAuthority<Context = RuntimeReplayIdentity>,
    {
        let snapshot = load_snapshot(store, &replay_identity(context))?;
        self.retain_product_with_snapshot(
            store,
            authority,
            context,
            Projection {
                job,
                source,
                product,
            },
            &snapshot,
        )
    }

    fn retain_product_with_snapshot<S, A>(
        &self,
        store: &mut S,
        authority: &A,
        context: StageRuntimeContext<'_>,
        projection: Projection<'_>,
        snapshot: &ReplaySnapshot,
    ) -> Result<StageRuntimeRetention, StageRuntimeProjectionError<A::Error, S::Error>>
    where
        S: ReplayStore,
        A: ReplayAuthority<Context = RuntimeReplayIdentity>,
    {
        let event_id = projected_event_id(projection.job, projection.source)
            .map_err(StageRuntimeProjectionError::Product)?;
        let existing = snapshot
            .events
            .iter()
            .find(|frame| frame.event_id == event_id.0);
        let message = if let Some(frame) = existing {
            let original: RuntimeEventMessage = serde_json::from_slice(&frame.frame)
                .map_err(|_| StageRuntimeProjectionError::CorruptOriginal)?;
            if !same_semantic_product(&original, projection.product) {
                return Ok(StageRuntimeRetention::Conflict {
                    highest_sequence: snapshot.highest_sequence,
                });
            }
            original
        } else {
            let sequence = snapshot
                .highest_sequence
                .checked_add(1)
                .ok_or(StageRuntimeProjectionError::InvalidSource)?;
            build_message(context, event_id, sequence, projection.product)?
        };
        let decision = self
            .responder
            .retain_runtime_event(store, authority, &message)
            .map_err(StageRuntimeProjectionError::Replay)?;
        Ok(match decision {
            ReplayDecision::Accepted { .. } => StageRuntimeRetention::Ready {
                message: Box::new(message),
                duplicate: false,
            },
            ReplayDecision::Duplicate { original, .. } => {
                let original = serde_json::from_slice(&original.frame)
                    .map_err(|_| StageRuntimeProjectionError::CorruptOriginal)?;
                StageRuntimeRetention::Ready {
                    message: Box::new(original),
                    duplicate: true,
                }
            }
            ReplayDecision::Gap {
                highest_sequence,
                replay_from_sequence,
            } => StageRuntimeRetention::Gap {
                highest_sequence,
                replay_from_sequence,
            },
            ReplayDecision::Conflict { highest_sequence } => {
                StageRuntimeRetention::Conflict { highest_sequence }
            }
        })
    }
}

fn build_message<AuthorityError, StoreError>(
    context: StageRuntimeContext<'_>,
    event_id: ExecutionEventId,
    sequence: u64,
    product: &PreparedStageProduct,
) -> Result<RuntimeEventMessage, StageRuntimeProjectionError<AuthorityError, StoreError>> {
    let sequence =
        i64::try_from(sequence).map_err(|_| StageRuntimeProjectionError::InvalidSource)?;
    let payload = EncodedPayload {
        content_type: product.media_type().to_owned(),
        data_base64: STANDARD.encode(product.bytes()),
        payload_digest: product.digest().clone(),
    };
    let message_id = canonical_id(
        "xmsg",
        b"codex-stage-product-message",
        &[event_id.0.as_bytes(), &sequence.to_be_bytes()],
    );
    Ok(RuntimeEventMessage {
        codex_thread_id: context.session_identity.codex_thread_id.clone(),
        event: ExecutionEventRecord {
            category: product.category().clone(),
            event_id,
            occurred_at: context.occurred_at.clone(),
            payload: Some(payload),
            sequence: ExecutionSequence(sequence),
            summary: product.summary().to_owned(),
        },
        kind: RuntimeEventMessageKind::RuntimeEvent,
        lease: context.lease.clone(),
        message_id: ExecutionMessageId(message_id),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: context.sent_at.clone(),
        session_identity: context.session_identity.clone(),
        worker_session_id: context.worker_session_id.clone(),
    })
}

fn projected_event_id(
    job: &ExecutionJob,
    source: &str,
) -> Result<ExecutionEventId, StageProductError> {
    let job_digest = stage_product_job_digest(job)?;
    Ok(ExecutionEventId(canonical_id(
        "xevt",
        b"codex-stage-product-event",
        &[job_digest.0.as_bytes(), source.as_bytes()],
    )))
}

fn replay_identity(context: StageRuntimeContext<'_>) -> RuntimeReplayIdentity {
    RuntimeReplayIdentity {
        lease: context.lease.clone(),
        worker_session_id: context.worker_session_id.clone(),
        session_identity: context.session_identity.clone(),
        codex_thread_id: context.session_identity.codex_thread_id.clone(),
    }
}

fn load_snapshot<S, AuthorityError>(
    store: &mut S,
    identity: &RuntimeReplayIdentity,
) -> Result<ReplaySnapshot, StageRuntimeProjectionError<AuthorityError, S::Error>>
where
    S: ReplayStore,
{
    store
        .load(&identity.stream_key())
        .map(Option::unwrap_or_default)
        .map_err(StageRuntimeProjectionError::Store)
}

fn same_semantic_product(message: &RuntimeEventMessage, product: &PreparedStageProduct) -> bool {
    message.event.category == *product.category()
        && message.event.summary == product.summary()
        && message.event.payload.as_ref().is_some_and(|payload| {
            payload.content_type == product.media_type()
                && payload.payload_digest == *product.digest()
                && payload.data_base64 == STANDARD.encode(product.bytes())
        })
}

fn snapshot_has_policy(snapshot: &ReplaySnapshot, policy_id: &ExecutionEventId) -> bool {
    snapshot.events.iter().any(|frame| {
        if frame.event_id != policy_id.0 {
            return false;
        }
        let Ok(message) = serde_json::from_slice::<RuntimeEventMessage>(&frame.frame) else {
            return false;
        };
        message.event.category == ExecutionEventCategory::Lifecycle
            && message.event.payload.as_ref().is_some_and(|payload| {
                decode_payload(payload).is_some_and(|bytes| {
                    serde_json::from_slice::<Value>(&bytes).is_ok_and(|value| {
                        value.get("protocol").and_then(Value::as_str)
                            == Some(VERIFICATION_POLICY_PROTOCOL)
                    })
                })
            })
    })
}

fn bind_verification_evidence<AuthorityError, StoreError>(
    final_message: &[u8],
    snapshot: &ReplaySnapshot,
) -> Result<Vec<u8>, StageRuntimeProjectionError<AuthorityError, StoreError>> {
    let result: ModelVerificationResult = serde_json::from_slice(final_message)
        .map_err(|_| StageRuntimeProjectionError::InvalidEvidence)?;
    if serde_json::to_vec(&result).ok().as_deref() != Some(final_message) {
        return Err(StageRuntimeProjectionError::InvalidEvidence);
    }
    let catalog = evidence_catalog(snapshot)?;
    let findings = result
        .findings
        .into_iter()
        .map(|finding| {
            let evidence_sources = finding
                .evidence_sources
                .into_iter()
                .map(|source| {
                    let evidence = catalog
                        .get(&source.source_id)
                        .filter(|evidence| evidence.kind == source.evidence_type)
                        .ok_or(StageRuntimeProjectionError::InvalidEvidence)?;
                    Ok(BoundVerificationEvidenceSource {
                        evidence_type: source.evidence_type,
                        event_id: evidence.event_id.clone(),
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(BoundVerificationFinding {
                finding_id: finding.finding_id,
                criterion_id: finding.criterion_id,
                verdict: finding.verdict,
                explanation: finding.explanation,
                evidence_sources,
            })
        })
        .collect::<Result<Vec<_>, StageRuntimeProjectionError<AuthorityError, StoreError>>>()?;
    serde_json::to_vec(&BoundVerificationResult {
        protocol: result.protocol,
        delivery_spec_id: result.delivery_spec_id,
        delivery_spec_revision: result.delivery_spec_revision,
        candidate_ref: result.candidate_ref,
        findings,
    })
    .map_err(|_| StageRuntimeProjectionError::InvalidEvidence)
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ModelVerificationResult {
    protocol: String,
    delivery_spec_id: String,
    delivery_spec_revision: u64,
    candidate_ref: String,
    findings: Vec<ModelVerificationFinding>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ModelVerificationFinding {
    finding_id: String,
    criterion_id: Option<String>,
    verdict: String,
    explanation: String,
    evidence_sources: Vec<ModelVerificationEvidenceSource>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ModelVerificationEvidenceSource {
    #[serde(rename = "type")]
    evidence_type: EvidenceKind,
    source_id: String,
}

#[derive(Serialize)]
struct BoundVerificationResult {
    protocol: String,
    delivery_spec_id: String,
    delivery_spec_revision: u64,
    candidate_ref: String,
    findings: Vec<BoundVerificationFinding>,
}

#[derive(Serialize)]
struct BoundVerificationFinding {
    finding_id: String,
    criterion_id: Option<String>,
    verdict: String,
    explanation: String,
    evidence_sources: Vec<BoundVerificationEvidenceSource>,
}

#[derive(Serialize)]
struct BoundVerificationEvidenceSource {
    #[serde(rename = "type")]
    evidence_type: EvidenceKind,
    event_id: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum EvidenceKind {
    Command,
    Test,
}

#[derive(Debug)]
struct EvidenceBinding {
    kind: EvidenceKind,
    event_id: String,
}

fn evidence_catalog<AuthorityError, StoreError>(
    snapshot: &ReplaySnapshot,
) -> Result<HashMap<String, EvidenceBinding>, StageRuntimeProjectionError<AuthorityError, StoreError>>
{
    let mut catalog = HashMap::new();
    for frame in &snapshot.events {
        let message: RuntimeEventMessage = serde_json::from_slice(&frame.frame)
            .map_err(|_| StageRuntimeProjectionError::CorruptOriginal)?;
        let kind = match message.event.category {
            ExecutionEventCategory::Command => EvidenceKind::Command,
            ExecutionEventCategory::Test => EvidenceKind::Test,
            _ => continue,
        };
        let payload = message
            .event
            .payload
            .as_ref()
            .filter(|payload| payload.content_type == JSON_CONTENT_TYPE)
            .and_then(decode_payload)
            .ok_or(StageRuntimeProjectionError::CorruptOriginal)?;
        let value: Value = serde_json::from_slice(&payload)
            .map_err(|_| StageRuntimeProjectionError::CorruptOriginal)?;
        let source_id = value
            .get("source_id")
            .and_then(Value::as_str)
            .ok_or(StageRuntimeProjectionError::CorruptOriginal)?;
        let previous = catalog.insert(
            source_id.to_owned(),
            EvidenceBinding {
                kind,
                event_id: message.event.event_id.0,
            },
        );
        if previous.is_some() {
            return Err(StageRuntimeProjectionError::InvalidEvidence);
        }
    }
    Ok(catalog)
}

fn decode_payload(payload: &EncodedPayload) -> Option<Vec<u8>> {
    let bytes = STANDARD.decode(&payload.data_base64).ok()?;
    let digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(&bytes)));
    (digest == payload.payload_digest).then_some(bytes)
}

fn classify_evidence(command: &[String]) -> VerificationEvidenceKind {
    let normalized = command
        .iter()
        .map(|part| part.to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join(" ");
    let test = [
        "cargo test",
        "cargo nextest",
        "pnpm test",
        "npm test",
        "npm run test",
        "yarn test",
        "bun test",
        "pytest",
        "python -m pytest",
        "go test",
        "dotnet test",
        "swift test",
        "gradle test",
        "mvn test",
    ]
    .iter()
    .any(|needle| normalized.contains(needle));
    if test {
        VerificationEvidenceKind::Test
    } else {
        VerificationEvidenceKind::Command
    }
}

fn verification_role(role: &str) -> bool {
    matches!(role, "reviewer" | "verifier" | "adversarial-verifier")
}

fn validate_source_id<AuthorityError, StoreError>(
    value: &str,
) -> Result<(), StageRuntimeProjectionError<AuthorityError, StoreError>> {
    if value.is_empty()
        || value.len() > MAX_SOURCE_ID_BYTES
        || value
            .bytes()
            .any(|byte| matches!(byte, 0..=8 | 11..=12 | 14..=31 | 127))
    {
        Err(StageRuntimeProjectionError::InvalidSource)
    } else {
        Ok(())
    }
}

pub(crate) fn source_key(kind: &str, turn_id: &str, call_id: Option<&str>) -> String {
    let mut value = format!("{kind}:{}:{turn_id}", turn_id.len());
    if let Some(call_id) = call_id {
        value.push(':');
        value.push_str(&call_id.len().to_string());
        value.push(':');
        value.push_str(call_id);
    }
    value
}

fn canonical_id(prefix: &str, namespace: &[u8], parts: &[&[u8]]) -> String {
    let mut digest = Sha256::new();
    digest.update(namespace);
    for part in parts {
        digest.update((part.len() as u64).to_be_bytes());
        digest.update(part);
    }
    let hex = format!("{:x}", digest.finalize());
    format!("{prefix}_{}", &hex[..26].to_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use winwincode_domain::{
        CodexThreadId, DeliveryId, DeliveryTaskId, ExecutionJobId, FencingToken, LeaseId,
        ProductSessionId, RepositoryId, StageRunId, WorkerId, WorkerInstanceId,
    };
    use winwincode_execution_port::{
        generated::{
            DeliveryStageAcceptanceCriterionInput, DeliveryStageExecutionScope,
            DeliveryStageExecutionScopeKind, DeliveryStageInput, DeliveryStageTaskInput,
            ExecutionLimits, ExecutionScope, ExecutionWorkspace, ExecutionWorkspaceWriteMode,
        },
        replay::{ReplayFrame, ReplayStreamKey},
    };

    use super::*;

    const CANDIDATE_REF: &str =
        "git-candidate:sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const PLANNER_JSON: &str = concat!(
        "{\"schemaVersion\":1,",
        "\"protocol\":\"winwincode.planner-solution.v1\",",
        "\"solution\":{",
        "\"id\":\"solution:fixture\",",
        "\"summary\":\"Implement the accepted change.\",",
        "\"approach\":[\"Change the source and run the exact check.\"],",
        "\"components\":[{",
        "\"id\":\"component:fixture\",",
        "\"label\":\"Fixture component\",",
        "\"responsibility\":\"Own the accepted source change.\",",
        "\"kind\":\"component\",",
        "\"trustBoundary\":\"repository\",",
        "\"unresolved\":false,",
        "\"repositoryPathPrefixes\":[\"src\"]",
        "}],",
        "\"connections\":[{",
        "\"id\":\"connection:fixture\",",
        "\"from\":\"platform:codex-core\",",
        "\"to\":\"component:fixture\",",
        "\"label\":\"implements\"",
        "}]",
        "},",
        "\"architectureDiagram\":{",
        "\"id\":\"diagram:architecture\",",
        "\"kind\":\"system-architecture\",",
        "\"title\":\"Fixture architecture\",",
        "\"nodes\":[{",
        "\"id\":\"diagram:architecture:stage\",",
        "\"label\":\"Implementation\",",
        "\"description\":\"Applies the accepted change.\",",
        "\"kind\":\"stage\",",
        "\"trustBoundary\":null,",
        "\"unresolved\":false",
        "}],",
        "\"edges\":[]",
        "},",
        "\"processDiagram\":{",
        "\"id\":\"diagram:process\",",
        "\"kind\":\"process-flow\",",
        "\"title\":\"Fixture process\",",
        "\"nodes\":[{",
        "\"id\":\"diagram:process:stage\",",
        "\"label\":\"Implementation\",",
        "\"description\":\"Applies and verifies the change.\",",
        "\"kind\":\"stage\",",
        "\"trustBoundary\":null,",
        "\"unresolved\":false",
        "}],",
        "\"edges\":[]",
        "},",
        "\"risks\":[\"The exact check may expose a regression.\"],",
        "\"unresolvedItems\":[],",
        "\"taskProposals\":[{",
        "\"id\":\"dtk_00000000000000000000000001\",",
        "\"title\":\"Implement fixture\",",
        "\"goal\":\"Implement fixture\",",
        "\"acceptanceCriterionIds\":[\"criterion-fixture\"],",
        "\"blockedByTaskIds\":[]",
        "}]",
        "}"
    );
    const VERIFICATION_RESULT: &str = concat!(
        "{\"protocol\":\"winwincode.independent-verification-result.v1\",",
        "\"delivery_spec_id\":\"spec-fixture\",",
        "\"delivery_spec_revision\":2,",
        "\"candidate_ref\":\"git-candidate:sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\",",
        "\"findings\":[{",
        "\"finding_id\":\"finding-fixture\",",
        "\"criterion_id\":\"criterion-fixture\",",
        "\"verdict\":\"pass\",",
        "\"explanation\":\"Direct command and test evidence passed.\",",
        "\"evidence_sources\":[",
        "{\"type\":\"test\",\"source_id\":\"call-test\"},",
        "{\"type\":\"command\",\"source_id\":\"call-command\"}",
        "]",
        "}]",
        "}"
    );

    #[derive(Default)]
    struct MemoryStore(BTreeMap<String, ReplaySnapshot>);

    impl ReplayStore for MemoryStore {
        type Error = &'static str;

        fn load(
            &mut self,
            stream: &ReplayStreamKey,
        ) -> Result<Option<ReplaySnapshot>, Self::Error> {
            Ok(self.0.get(stream.as_str()).cloned())
        }

        fn append(
            &mut self,
            stream: &ReplayStreamKey,
            expected_highest_sequence: u64,
            frame: &ReplayFrame,
        ) -> Result<(), Self::Error> {
            let snapshot = self.0.entry(stream.as_str().to_owned()).or_default();
            if snapshot.highest_sequence != expected_highest_sequence
                || frame.sequence != expected_highest_sequence + 1
            {
                return Err("conflict");
            }
            snapshot.highest_sequence = frame.sequence;
            snapshot.events.push(frame.clone());
            snapshot.validate().map_err(|_| "corrupt")
        }
    }

    struct AllowAuthority;

    impl ReplayAuthority for AllowAuthority {
        type Context = RuntimeReplayIdentity;
        type Error = &'static str;

        fn validate_active_lease(
            &self,
            _stream: &ReplayStreamKey,
            _context: &Self::Context,
        ) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    struct Fixture {
        lease: ExecutionLeaseStamp,
        worker_session_id: WorkerSessionId,
        session_identity: SessionIdentity,
        occurred_at: Instant,
        sent_at: Instant,
    }

    impl Fixture {
        fn context(&self) -> StageRuntimeContext<'_> {
            StageRuntimeContext {
                lease: &self.lease,
                worker_session_id: &self.worker_session_id,
                session_identity: &self.session_identity,
                occurred_at: &self.occurred_at,
                sent_at: &self.sent_at,
            }
        }
    }

    fn fixture() -> Fixture {
        let worker_session_id = WorkerSessionId("wsn_00000000000000000000000001".to_owned());
        Fixture {
            lease: ExecutionLeaseStamp {
                attempt: 1,
                expires_at: Instant("2026-08-28T01:00:00.000Z".to_owned()),
                fencing_token: FencingToken("1".to_owned()),
                issued_at: Instant("2026-08-28T00:00:00.000Z".to_owned()),
                job_id: ExecutionJobId("job_00000000000000000000000001".to_owned()),
                lease_id: LeaseId("lse_00000000000000000000000001".to_owned()),
                worker_id: WorkerId("wrk_00000000000000000000000001".to_owned()),
                worker_instance_id: WorkerInstanceId("wki_00000000000000000000000001".to_owned()),
            },
            session_identity: SessionIdentity {
                codex_thread_id: CodexThreadId("cdx_00000000000000000000000001".to_owned()),
                product_session_id: ProductSessionId("psn_00000000000000000000000001".to_owned()),
                stage_run_id: Some(StageRunId("run_00000000000000000000000001".to_owned())),
                worker_session_id: worker_session_id.clone(),
            },
            worker_session_id,
            occurred_at: Instant("2026-08-28T00:00:00.000Z".to_owned()),
            sent_at: Instant("2026-08-28T00:00:00.000Z".to_owned()),
        }
    }

    fn job(role: &str) -> ExecutionJob {
        let task_role = role != "planner";
        let task_id = DeliveryTaskId("dtk_00000000000000000000000001".to_owned());
        ExecutionJob {
            attempt: 1,
            execution_profile: role.to_owned(),
            goal: "Implement fixture".to_owned(),
            job_id: ExecutionJobId("job_00000000000000000000000001".to_owned()),
            limits: ExecutionLimits {
                deadline_at: Instant("2026-08-28T01:00:00.000Z".to_owned()),
                max_artifact_bytes: 1_048_576,
                max_runtime_seconds: 300,
            },
            payload_digest: Sha256Digest(format!("sha256:{}", "b".repeat(64))),
            scope: ExecutionScope::DeliveryStageExecutionScope(DeliveryStageExecutionScope {
                delivery_id: DeliveryId("dlv_00000000000000000000000001".to_owned()),
                delivery_task_id: task_role.then(|| task_id.clone()),
                kind: DeliveryStageExecutionScopeKind::DeliveryStage,
                product_session_id: ProductSessionId("psn_00000000000000000000000001".to_owned()),
                rework_authorization: None,
                stage_run_id: StageRunId("run_00000000000000000000000001".to_owned()),
            }),
            stage_input: Some(DeliveryStageInput {
                acceptance_criteria: vec![DeliveryStageAcceptanceCriterionInput {
                    criterion_id: "criterion-fixture".to_owned(),
                    description: "The exact fixture behavior is verified.".to_owned(),
                    required: true,
                    verification_method: Some("Run the exact fixture check.".to_owned()),
                }],
                candidate_ref: task_role.then(|| CANDIDATE_REF.to_owned()),
                constraints: vec!["Keep the exact repository boundary.".to_owned()],
                delivery_spec_id: "spec-fixture".to_owned(),
                delivery_spec_revision: 2,
                goal: "Implement fixture".to_owned(),
                out_of_scope: Vec::new(),
                schema_version: SchemaVersion::WinwincodeV1,
                scope: vec!["Fixture source".to_owned()],
                task: task_role.then(|| DeliveryStageTaskInput {
                    acceptance_criterion_ids: vec!["criterion-fixture".to_owned()],
                    goal: "Implement fixture".to_owned(),
                    task_id,
                    title: "Fixture task".to_owned(),
                }),
                title: "Fixture Delivery".to_owned(),
            }),
            workspace: ExecutionWorkspace {
                checkout_revision: "main".to_owned(),
                repository_id: RepositoryId("repo_00000000000000000000000001".to_owned()),
                write_mode: if matches!(role, "executor" | "remediator") {
                    ExecutionWorkspaceWriteMode::Candidate
                } else {
                    ExecutionWorkspaceWriteMode::ReadOnly
                },
            },
        }
    }

    fn ready(retention: Option<StageRuntimeRetention>) -> Box<RuntimeEventMessage> {
        let Some(StageRuntimeRetention::Ready { message, .. }) = retention else {
            panic!("expected a ready stage product")
        };
        message
    }

    fn retain_verification_sequence(
        projector: &StageRuntimeProjector,
        store: &mut MemoryStore,
        fixture: &Fixture,
        job: &ExecutionJob,
    ) -> [Box<RuntimeEventMessage>; 4] {
        let policy = ready(
            projector
                .retain_turn_started(
                    store,
                    &AllowAuthority,
                    fixture.context(),
                    job,
                    "turn-verifier",
                )
                .expect("retain policy"),
        );
        let evidence = [
            (["cargo".to_owned(), "test".to_owned()], "call-test"),
            (["git".to_owned(), "status".to_owned()], "call-command"),
        ]
        .map(|(command, call_id)| {
            ready(
                projector
                    .retain_exec_command_end(
                        store,
                        &AllowAuthority,
                        fixture.context(),
                        job,
                        StageCommandEnd {
                            command: &command,
                            turn_id: "turn-verifier",
                            call_id,
                            status: VerificationEvidenceStatus::Completed,
                            exit_code: 0,
                        },
                    )
                    .expect("retain verification evidence"),
            )
        });
        let [test, command] = evidence;
        let result = ready(
            projector
                .retain_turn_completed(
                    store,
                    &AllowAuthority,
                    fixture.context(),
                    job,
                    StageTurnCompletion {
                        turn_id: "turn-verifier",
                        final_message: Some(VERIFICATION_RESULT),
                        failed: false,
                    },
                )
                .expect("retain verification result"),
        );
        [policy, test, command, result]
    }

    #[test]
    fn planner_final_is_strict_and_restarts_with_original_frame() {
        let fixture = fixture();
        let mut store = MemoryStore::default();
        let projector = StageRuntimeProjector::new();
        let job = job("planner");
        let first = ready(
            projector
                .retain_turn_completed(
                    &mut store,
                    &AllowAuthority,
                    fixture.context(),
                    &job,
                    StageTurnCompletion {
                        turn_id: "turn-planner",
                        final_message: Some(PLANNER_JSON),
                        failed: false,
                    },
                )
                .expect("retain planner product"),
        );
        assert_eq!(first.event.sequence.0, 1);
        assert_eq!(first.event.category, ExecutionEventCategory::Activity);
        let restarted = StageRuntimeProjector::new();
        let second = restarted
            .retain_turn_completed(
                &mut store,
                &AllowAuthority,
                fixture.context(),
                &job,
                StageTurnCompletion {
                    turn_id: "turn-planner",
                    final_message: Some(PLANNER_JSON),
                    failed: false,
                },
            )
            .expect("restart planner product");
        let Some(StageRuntimeRetention::Ready {
            message,
            duplicate: true,
        }) = second
        else {
            panic!("expected exact duplicate")
        };
        assert_eq!(message, first);
        assert!(
            projector
                .retain_turn_completed(
                    &mut store,
                    &AllowAuthority,
                    fixture.context(),
                    &job,
                    StageTurnCompletion {
                        turn_id: "turn-loopback",
                        final_message: Some("LOOPBACK_RESPONSE"),
                        failed: false,
                    },
                )
                .is_err()
        );
    }

    #[test]
    fn verification_policy_evidence_and_result_are_contiguous_and_exact() {
        let fixture = fixture();
        let mut store = MemoryStore::default();
        let projector = StageRuntimeProjector::new();
        let job = job("verifier");
        assert!(matches!(
            projector.retain_exec_command_end(
                &mut store,
                &AllowAuthority,
                fixture.context(),
                &job,
                StageCommandEnd {
                    command: &["cargo".to_owned(), "test".to_owned()],
                    turn_id: "turn-verifier",
                    call_id: "call-test",
                    status: VerificationEvidenceStatus::Completed,
                    exit_code: 0,
                },
            ),
            Err(StageRuntimeProjectionError::InvalidEvidence)
        ));
        let [policy, test, command, result] =
            retain_verification_sequence(&projector, &mut store, &fixture, &job);
        assert_eq!(
            [
                policy.event.sequence.0,
                test.event.sequence.0,
                command.event.sequence.0,
                result.event.sequence.0,
            ],
            [1, 2, 3, 4]
        );
        assert_eq!(test.event.category, ExecutionEventCategory::Test);
        assert_eq!(command.event.category, ExecutionEventCategory::Command);
        let payload = result.event.payload.as_ref().expect("result payload");
        let bytes = STANDARD.decode(&payload.data_base64).expect("result bytes");
        let json: Value = serde_json::from_slice(&bytes).expect("result JSON");
        let sources = json["findings"][0]["evidence_sources"]
            .as_array()
            .expect("evidence sources");
        assert_eq!(sources[0]["event_id"], test.event.event_id.0);
        assert_eq!(sources[1]["event_id"], command.event.event_id.0);
        assert!(
            sources
                .iter()
                .all(|source| source.get("source_id").is_none())
        );

        let restarted = StageRuntimeProjector::new();
        let duplicate = restarted
            .retain_turn_completed(
                &mut store,
                &AllowAuthority,
                fixture.context(),
                &job,
                StageTurnCompletion {
                    turn_id: "turn-verifier",
                    final_message: Some(VERIFICATION_RESULT),
                    failed: false,
                },
            )
            .expect("restart verification result");
        let Some(StageRuntimeRetention::Ready {
            message,
            duplicate: true,
        }) = duplicate
        else {
            panic!("expected exact duplicate")
        };
        assert_eq!(message, result);
    }

    #[test]
    fn verification_changed_or_foreign_evidence_fails_closed() {
        let fixture = fixture();
        let mut store = MemoryStore::default();
        let projector = StageRuntimeProjector::new();
        let job = job("reviewer");
        ready(
            projector
                .retain_turn_started(
                    &mut store,
                    &AllowAuthority,
                    fixture.context(),
                    &job,
                    "turn-reviewer",
                )
                .expect("retain policy"),
        );
        let first = ready(
            projector
                .retain_exec_command_end(
                    &mut store,
                    &AllowAuthority,
                    fixture.context(),
                    &job,
                    StageCommandEnd {
                        command: &["git".to_owned(), "status".to_owned()],
                        turn_id: "turn-reviewer",
                        call_id: "call-command",
                        status: VerificationEvidenceStatus::Completed,
                        exit_code: 0,
                    },
                )
                .expect("retain evidence"),
        );
        assert!(matches!(
            projector
                .retain_exec_command_end(
                    &mut store,
                    &AllowAuthority,
                    fixture.context(),
                    &job,
                    StageCommandEnd {
                        command: &["git".to_owned(), "status".to_owned()],
                        turn_id: "turn-reviewer",
                        call_id: "call-command",
                        status: VerificationEvidenceStatus::Failed,
                        exit_code: 1,
                    },
                )
                .expect("changed duplicate is a typed conflict"),
            Some(StageRuntimeRetention::Conflict { .. })
        ));
        assert_eq!(first.event.sequence.0, 2);
        let foreign = VERIFICATION_RESULT.replace("call-test", "foreign-call");
        assert!(matches!(
            projector.retain_turn_completed(
                &mut store,
                &AllowAuthority,
                fixture.context(),
                &job,
                StageTurnCompletion {
                    turn_id: "turn-reviewer",
                    final_message: Some(&foreign),
                    failed: false,
                },
            ),
            Err(StageRuntimeProjectionError::InvalidEvidence)
        ));
        assert!(
            projector
                .retain_turn_completed(
                    &mut store,
                    &AllowAuthority,
                    fixture.context(),
                    &job,
                    StageTurnCompletion {
                        turn_id: "turn-reviewer",
                        final_message: None,
                        failed: false,
                    },
                )
                .is_err()
        );
        assert_eq!(
            projector
                .retain_turn_completed(
                    &mut store,
                    &AllowAuthority,
                    fixture.context(),
                    &job,
                    StageTurnCompletion {
                        turn_id: "turn-reviewer",
                        final_message: Some(VERIFICATION_RESULT),
                        failed: true,
                    },
                )
                .expect("failed turn has no product"),
            None
        );
    }
}
