// SPDX-License-Identifier: Apache-2.0

//! Worker runtime `TraceOutbox` and content-addressed Artifact reference flow.
//!
//! Runtime facts are reduced to typed, secret-safe metadata and Artifact
//! references before they enter the existing durable replay store. Large bytes
//! go directly to an injected fenced Artifact cache; this module neither owns
//! another Artifact store nor creates a product or Control Plane event cursor.

use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_domain::{
    ArtifactId, ExecutionEventId, ExecutionMessageId, ExecutionSequence, Instant, SchemaVersion,
    SessionIdentity, Sha256Digest, WorkerSessionId,
};

use crate::action_gateway::{
    ExecutionEnvelopeToken, GateDecision, GateInput, PreActionDecisionRecorder,
};
use crate::action_normalizer::{
    ActionObject, ActionOperation, ActionRisk, ActionScope, ActionSource,
};
use crate::generated::{
    ArtifactDescriptor, ArtifactKind, ArtifactReference, EncodedPayload, ExecutionEventCategory,
    ExecutionEventRecord, ExecutionLeaseStamp, RuntimeAckMessage, RuntimeEventMessage,
    RuntimeEventMessageKind, RuntimeReplayRequestMessage,
};
use crate::replay::{ReplayAcknowledgementStore, ReplayAuthority, ReplayDecision, ReplayStore};
use crate::runtime_replay::{
    RuntimeReplayAckReceipt, RuntimeReplayBatch, RuntimeReplayError, RuntimeReplayIdentity,
    RuntimeReplayResponder,
};

const TRACE_PAYLOAD_SCHEMA_VERSION: &str = "winwincode.runtime-trace.v1";
const TRACE_PAYLOAD_CONTENT_TYPE: &str = "application/vnd.winwincode.runtime-trace+json";
const MAX_SAFE_SEQUENCE: i64 = 9_007_199_254_740_991;
const MAX_SUMMARY_BYTES: usize = 2_000;
const MAX_ARTIFACT_BYTES: usize = 1_099_511_627_776;

/// Secret-safe summary accepted into a runtime event row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretSafeTraceSummary(String);

impl SecretSafeTraceSummary {
    /// Validates a bounded summary which contains no common credential form.
    ///
    /// # Errors
    ///
    /// Rejects blank, oversized, multiline, control-character, or visibly
    /// credential-bearing summaries.
    pub fn new(value: impl Into<String>) -> Result<Self, RuntimeTraceInputError> {
        let value = value.into();
        let normalized = value.to_ascii_lowercase();
        if value.trim().is_empty() {
            return Err(RuntimeTraceInputError::InvalidSummary);
        }
        if value.len() > MAX_SUMMARY_BYTES
            || value.chars().any(char::is_control)
            || [
                "authorization:",
                "bearer ",
                "password=",
                "password:",
                "secret=",
                "secret:",
                "token=",
                "token:",
                "api_key",
                "apikey",
                "private key",
            ]
            .iter()
            .any(|marker| normalized.contains(marker))
        {
            return Err(RuntimeTraceInputError::UnsafeSummary);
        }
        Ok(Self(value))
    }

    /// Returns the validated summary.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Stage transition retained as a typed runtime fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageTraceState {
    Started,
    Paused,
    Resumed,
    Completed,
    Failed,
}

/// Tool result retained without command output or request arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolTraceOutcome {
    Succeeded,
    Failed,
    Cancelled,
}

/// Worker lifecycle fact retained without provider or tool payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerRuntimeTraceState {
    Started,
    Checkpointed,
    Disconnected,
    Reconnected,
    Parked,
    Stopped,
}

/// Process-level execution strategy selected before a Job starts.
///
/// PR0 keeps [`Self::React`] as the default. The delegated variants are
/// feature gates only until their deterministic executors are introduced by
/// later changes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    #[default]
    React,
    DelegatedPatchShadow,
    DelegatedPatch,
}

impl ExecutionMode {
    /// Parses the canonical configuration spelling.
    #[must_use]
    pub fn from_config(value: &str) -> Option<Self> {
        match value {
            "react" => Some(Self::React),
            "delegated_patch_shadow" => Some(Self::DelegatedPatchShadow),
            "delegated_patch" => Some(Self::DelegatedPatch),
            _ => None,
        }
    }

    /// Returns the canonical configuration spelling.
    #[must_use]
    pub const fn as_config(self) -> &'static str {
        match self {
            Self::React => "react",
            Self::DelegatedPatchShadow => "delegated_patch_shadow",
            Self::DelegatedPatch => "delegated_patch",
        }
    }
}

/// Policy controlling when the optional one-shot Observer may run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObserverMode {
    #[default]
    Off,
    Shadow,
    AmbiguousOnly,
    Always,
}

impl ObserverMode {
    /// Parses the canonical configuration spelling.
    #[must_use]
    pub fn from_config(value: &str) -> Option<Self> {
        match value {
            "off" => Some(Self::Off),
            "shadow" => Some(Self::Shadow),
            "ambiguous_only" => Some(Self::AmbiguousOnly),
            "always" => Some(Self::Always),
            _ => None,
        }
    }

    /// Returns the canonical configuration spelling.
    #[must_use]
    pub const fn as_config(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Shadow => "shadow",
            Self::AmbiguousOnly => "ambiguous_only",
            Self::Always => "always",
        }
    }
}

/// Secret-safe aggregate used to compare one execution attempt with another.
///
/// Only bounded counters and durations are retained. Provider content, tool
/// arguments, commands, paths, patches, source and logs are deliberately
/// absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PerformanceBaselineReport {
    pub execution_mode: ExecutionMode,
    pub observer_mode: ObserverMode,
    pub primary_model_call_count: i64,
    pub primary_model_input_tokens: i64,
    pub primary_model_cached_tokens: i64,
    pub primary_model_output_tokens: i64,
    pub primary_model_wait_ms: i64,
    pub tool_call_count: i64,
    pub patch_call_count: i64,
    pub patch_apply_ms: i64,
    pub files_changed: i64,
    pub validation_ms: i64,
    pub observer_call_count: i64,
    pub observer_wait_ms: i64,
    pub repair_rounds: i64,
    pub turn_count: i64,
    pub total_runtime_ms: i64,
}

/// Stable gate outcome retained without free-form policy details.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceGateOutcome {
    Allow,
    AllowWithWatch,
    RequestPlanDelta,
    PauseForHuman,
    DenyAction,
    ReplanRequired,
}

impl From<&GateDecision> for TraceGateOutcome {
    fn from(value: &GateDecision) -> Self {
        match value {
            GateDecision::Allow => Self::Allow,
            GateDecision::AllowWithWatch { .. } => Self::AllowWithWatch,
            GateDecision::RequestPlanDelta { .. } => Self::RequestPlanDelta,
            GateDecision::PauseForHuman { .. } => Self::PauseForHuman,
            GateDecision::DenyAction { .. } => Self::DenyAction,
            GateDecision::ReplanRequired { .. } => Self::ReplanRequired,
        }
    }
}

impl TraceGateOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::AllowWithWatch => "allow_with_watch",
            Self::RequestPlanDelta => "request_plan_delta",
            Self::PauseForHuman => "pause_for_human",
            Self::DenyAction => "deny_action",
            Self::ReplanRequired => "replan_required",
        }
    }
}

/// Typed trace data. It has no raw paths, arguments, environment, output, or bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RuntimeTraceFact {
    Stage {
        state: StageTraceState,
    },
    Action {
        source: ActionSource,
        objects: Vec<ActionObject>,
        operation: ActionOperation,
        scope: ActionScope,
        minimum_risk: ActionRisk,
    },
    Gate {
        source: ActionSource,
        operation: ActionOperation,
        decision: TraceGateOutcome,
        envelope_version: u64,
        envelope_digest: Sha256Digest,
    },
    Tool {
        source: ActionSource,
        outcome: ToolTraceOutcome,
    },
    Candidate {
        digest: Sha256Digest,
    },
    Runtime {
        state: WorkerRuntimeTraceState,
    },
    PerformanceBaseline {
        report: PerformanceBaselineReport,
    },
}

/// Exact JSON payload retained in a runtime event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimeTracePayload {
    pub schema_version: String,
    pub fact: RuntimeTraceFact,
    /// Content-addressed references only; Artifact bytes never enter this payload.
    pub artifacts: Vec<ArtifactReference>,
}

/// Message and source identity assigned to one runtime trace fact.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeTraceIdentity {
    pub lease: ExecutionLeaseStamp,
    pub worker_session_id: WorkerSessionId,
    pub session_identity: SessionIdentity,
    pub message_id: ExecutionMessageId,
    pub event_id: ExecutionEventId,
    pub sequence: ExecutionSequence,
    pub occurred_at: Instant,
    pub sent_at: Instant,
}

/// One typed trace fact ready for the Worker outbox.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeTraceDraft {
    pub identity: RuntimeTraceIdentity,
    pub category: ExecutionEventCategory,
    pub summary: SecretSafeTraceSummary,
    pub fact: RuntimeTraceFact,
    pub artifacts: Vec<ArtifactReference>,
}

impl RuntimeTraceDraft {
    /// Creates a secret-safe normalized Action fact without targets or tool payloads.
    ///
    /// # Errors
    ///
    /// Returns an error if the derived secret-safe summary cannot be built.
    pub fn gateway_action<Policy>(
        identity: RuntimeTraceIdentity,
        input: &GateInput<'_, Policy>,
    ) -> Result<Self, RuntimeTraceInputError> {
        let summary = SecretSafeTraceSummary::new(format!(
            "action {:?} {:?}",
            input.observed.source, input.observed.operation
        ))?;
        Ok(Self {
            identity,
            category: ExecutionEventCategory::Activity,
            summary,
            fact: RuntimeTraceFact::Action {
                source: input.observed.source,
                objects: input.observed.objects.clone(),
                operation: input.observed.operation,
                scope: input.observed.scope,
                minimum_risk: input.observed.minimum_risk,
            },
            artifacts: Vec::new(),
        })
    }

    /// Creates the mandatory pre-action control event for an Action Gateway decision.
    ///
    /// # Errors
    ///
    /// Returns an error if the derived secret-safe summary cannot be built.
    pub fn gateway_decision<Policy>(
        identity: RuntimeTraceIdentity,
        input: &GateInput<'_, Policy>,
        decision: &GateDecision,
    ) -> Result<Self, RuntimeTraceInputError> {
        let outcome = TraceGateOutcome::from(decision);
        let summary = SecretSafeTraceSummary::new(format!(
            "gate {} for {:?} {:?}",
            outcome.as_str(),
            input.observed.source,
            input.observed.operation
        ))?;
        Ok(Self {
            identity,
            category: ExecutionEventCategory::Activity,
            summary,
            fact: RuntimeTraceFact::Gate {
                source: input.observed.source,
                operation: input.observed.operation,
                decision: outcome,
                envelope_version: input.envelope.token.version,
                envelope_digest: input.envelope.token.digest.clone(),
            },
            artifacts: Vec::new(),
        })
    }
}

/// Trace input rejected before lease authority or durable storage is called.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeTraceInputError {
    InvalidSummary,
    UnsafeSummary,
    InvalidIdentity,
    InvalidSequence,
    InvalidArtifactReference,
    ConflictingArtifactReference,
    InvalidCandidateDigest,
    Serialization,
}

impl fmt::Display for RuntimeTraceInputError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidSummary => "runtime trace summary is empty",
            Self::UnsafeSummary => "runtime trace summary is not secret-safe",
            Self::InvalidIdentity => "runtime trace identity is invalid",
            Self::InvalidSequence => "runtime trace sequence is invalid",
            Self::InvalidArtifactReference => "runtime trace Artifact reference is invalid",
            Self::ConflictingArtifactReference => {
                "runtime trace contains conflicting Artifact references"
            }
            Self::InvalidCandidateDigest => "runtime trace candidate digest is invalid",
            Self::Serialization => "runtime trace payload cannot be encoded",
        })
    }
}

impl std::error::Error for RuntimeTraceInputError {}

/// Result of retaining a trace fact in the existing Worker replay store.
#[derive(Debug, Clone, PartialEq)]
pub enum RuntimeTraceRetention {
    /// A sendable original message is durable. Duplicate retries return the
    /// first stored message rather than rebuilding it.
    Ready {
        message: Box<RuntimeEventMessage>,
        highest_sequence: u64,
        duplicate: bool,
    },
    /// One or more prior trace facts must be replayed first.
    Gap {
        highest_sequence: u64,
        replay_from_sequence: u64,
    },
    /// The event id or sequence was reused with another semantic body.
    Conflict { highest_sequence: u64 },
}

/// Trace retention failure from validation, authority, persistence, or recovery.
#[derive(Debug, PartialEq, Eq)]
pub enum RuntimeTraceOutboxError<AuthorityError, StoreError> {
    Input(RuntimeTraceInputError),
    Replay(RuntimeReplayError<AuthorityError, StoreError>),
    CorruptOriginalFrame,
}

impl<AuthorityError: fmt::Debug, StoreError: fmt::Debug> fmt::Display
    for RuntimeTraceOutboxError<AuthorityError, StoreError>
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Input(error) => error.fmt(formatter),
            Self::Replay(error) => write!(formatter, "runtime trace replay failed: {error}"),
            Self::CorruptOriginalFrame => {
                formatter.write_str("runtime trace original frame is corrupt")
            }
        }
    }
}

impl<AuthorityError: fmt::Debug, StoreError: fmt::Debug> std::error::Error
    for RuntimeTraceOutboxError<AuthorityError, StoreError>
{
}

/// Stateless coordinator over the existing Worker replay store.
#[derive(Debug, Clone, Copy, Default)]
pub struct WorkerRuntimeTraceOutbox {
    responder: RuntimeReplayResponder,
}

impl WorkerRuntimeTraceOutbox {
    /// Creates a coordinator with no in-memory cursor or product authority.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            responder: RuntimeReplayResponder::new(),
        }
    }

    /// Reduces and durably retains one runtime fact before it is sent.
    ///
    /// # Errors
    ///
    /// Returns before durable mutation for unsafe summaries, malformed
    /// identities, invalid references, or serialization failures. Authority and
    /// store errors are forwarded by the existing replay state machine.
    pub fn retain<S, A>(
        &self,
        store: &mut S,
        authority: &A,
        draft: RuntimeTraceDraft,
    ) -> Result<RuntimeTraceRetention, RuntimeTraceOutboxError<A::Error, S::Error>>
    where
        S: ReplayStore,
        A: ReplayAuthority<Context = RuntimeReplayIdentity>,
    {
        let message = build_message(draft).map_err(RuntimeTraceOutboxError::Input)?;
        let decision = self
            .responder
            .retain_runtime_event(store, authority, &message)
            .map_err(RuntimeTraceOutboxError::Replay)?;
        match decision {
            ReplayDecision::Accepted { highest_sequence } => Ok(RuntimeTraceRetention::Ready {
                message: Box::new(message),
                highest_sequence,
                duplicate: false,
            }),
            ReplayDecision::Duplicate {
                highest_sequence,
                original,
            } => {
                let message = serde_json::from_slice(&original.frame)
                    .map_err(|_| RuntimeTraceOutboxError::CorruptOriginalFrame)?;
                Ok(RuntimeTraceRetention::Ready {
                    message: Box::new(message),
                    highest_sequence,
                    duplicate: true,
                })
            }
            ReplayDecision::Gap {
                highest_sequence,
                replay_from_sequence,
            } => Ok(RuntimeTraceRetention::Gap {
                highest_sequence,
                replay_from_sequence,
            }),
            ReplayDecision::Conflict { highest_sequence } => {
                Ok(RuntimeTraceRetention::Conflict { highest_sequence })
            }
        }
    }

    /// Replays original trace messages after a reconnect or Worker restart.
    ///
    /// # Errors
    ///
    /// Forwards request, authority, durable-state, and original-frame errors
    /// from the existing runtime replay responder.
    pub fn resume<S, A>(
        &self,
        store: &mut S,
        authority: &A,
        request: &RuntimeReplayRequestMessage,
    ) -> Result<RuntimeReplayBatch, RuntimeReplayError<A::Error, S::Error>>
    where
        S: ReplayStore,
        A: ReplayAuthority<Context = RuntimeReplayIdentity>,
    {
        self.responder.resume(store, authority, request)
    }

    /// Applies the Control Plane acknowledgement to the existing Worker cursor.
    ///
    /// # Errors
    ///
    /// Forwards acknowledgement, authority, and durable-state errors from the
    /// existing runtime replay responder.
    pub fn acknowledge<S, A>(
        &self,
        store: &mut S,
        authority: &A,
        acknowledgement: &RuntimeAckMessage,
    ) -> Result<RuntimeReplayAckReceipt, RuntimeReplayError<A::Error, S::Error>>
    where
        S: ReplayAcknowledgementStore,
        A: ReplayAuthority<Context = RuntimeReplayIdentity>,
    {
        self.responder
            .acknowledge(store, authority, acknowledgement)
    }
}

fn build_message(
    mut draft: RuntimeTraceDraft,
) -> Result<RuntimeEventMessage, RuntimeTraceInputError> {
    validate_trace_identity(&draft.identity)?;
    validate_trace_fact(&draft.fact)?;
    normalize_artifact_references(&mut draft.artifacts)?;
    let payload = RuntimeTracePayload {
        schema_version: TRACE_PAYLOAD_SCHEMA_VERSION.to_owned(),
        fact: draft.fact,
        artifacts: draft.artifacts,
    };
    let payload_json =
        serde_json::to_vec(&payload).map_err(|_| RuntimeTraceInputError::Serialization)?;
    let payload_digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(&payload_json)));
    Ok(RuntimeEventMessage {
        codex_thread_id: draft.identity.session_identity.codex_thread_id.clone(),
        event: ExecutionEventRecord {
            category: draft.category,
            event_id: draft.identity.event_id,
            occurred_at: draft.identity.occurred_at,
            payload: Some(EncodedPayload {
                content_type: TRACE_PAYLOAD_CONTENT_TYPE.to_owned(),
                data_base64: encode_base64(&payload_json),
                payload_digest,
            }),
            sequence: draft.identity.sequence,
            summary: draft.summary.0,
        },
        kind: RuntimeEventMessageKind::RuntimeEvent,
        lease: draft.identity.lease,
        message_id: draft.identity.message_id,
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: draft.identity.sent_at,
        session_identity: draft.identity.session_identity,
        worker_session_id: draft.identity.worker_session_id,
    })
}

fn validate_trace_identity(identity: &RuntimeTraceIdentity) -> Result<(), RuntimeTraceInputError> {
    if !canonical_id(&identity.message_id.0, "xmsg_")
        || !canonical_id(&identity.event_id.0, "xevt_")
        || identity.sequence.0 <= 0
        || identity.sequence.0 > MAX_SAFE_SEQUENCE
    {
        return Err(RuntimeTraceInputError::InvalidIdentity);
    }
    if identity.session_identity.worker_session_id != identity.worker_session_id {
        return Err(RuntimeTraceInputError::InvalidIdentity);
    }
    Ok(())
}

fn validate_trace_fact(fact: &RuntimeTraceFact) -> Result<(), RuntimeTraceInputError> {
    match fact {
        RuntimeTraceFact::Action { objects, .. } if objects.is_empty() => {
            return Err(RuntimeTraceInputError::InvalidIdentity);
        }
        RuntimeTraceFact::Gate {
            envelope_version,
            envelope_digest,
            ..
        } if *envelope_version == 0 || !sha256_digest(&envelope_digest.0) => {
            return Err(RuntimeTraceInputError::InvalidIdentity);
        }
        RuntimeTraceFact::Candidate { digest } if !sha256_digest(&digest.0) => {
            return Err(RuntimeTraceInputError::InvalidCandidateDigest);
        }
        RuntimeTraceFact::PerformanceBaseline { report }
            if performance_values(report)
                .any(|value| !(0..=MAX_SAFE_SEQUENCE).contains(&value)) =>
        {
            return Err(RuntimeTraceInputError::InvalidIdentity);
        }
        _ => {}
    }
    Ok(())
}

fn performance_values(report: &PerformanceBaselineReport) -> impl Iterator<Item = i64> {
    [
        report.primary_model_call_count,
        report.primary_model_input_tokens,
        report.primary_model_cached_tokens,
        report.primary_model_output_tokens,
        report.primary_model_wait_ms,
        report.tool_call_count,
        report.patch_call_count,
        report.patch_apply_ms,
        report.files_changed,
        report.validation_ms,
        report.observer_call_count,
        report.observer_wait_ms,
        report.repair_rounds,
        report.turn_count,
        report.total_runtime_ms,
    ]
    .into_iter()
}

fn normalize_artifact_references(
    references: &mut [ArtifactReference],
) -> Result<(), RuntimeTraceInputError> {
    references.sort_by(|left, right| left.artifact_id.0.cmp(&right.artifact_id.0));
    for reference in references.iter() {
        if !canonical_id(&reference.artifact_id.0, "art_") || !sha256_digest(&reference.digest.0) {
            return Err(RuntimeTraceInputError::InvalidArtifactReference);
        }
    }
    for pair in references.windows(2) {
        if pair[0].artifact_id == pair[1].artifact_id {
            if pair[0].digest != pair[1].digest {
                return Err(RuntimeTraceInputError::ConflictingArtifactReference);
            }
            return Err(RuntimeTraceInputError::InvalidArtifactReference);
        }
    }
    Ok(())
}

fn canonical_id(value: &str, prefix: &str) -> bool {
    value
        .strip_prefix(prefix)
        .is_some_and(|suffix| suffix.len() == 26 && suffix.bytes().all(crockford_base32))
}

fn crockford_base32(byte: u8) -> bool {
    byte.is_ascii_digit()
        || matches!(
            byte,
            b'A'..=b'H' | b'J' | b'K' | b'M' | b'N' | b'P'..=b'T' | b'V'..=b'Z'
        )
}

fn sha256_digest(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn encode_base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        encoded.push(char::from(ALPHABET[usize::from(first >> 2)]));
        encoded.push(char::from(
            ALPHABET[usize::from(((first & 0b0000_0011) << 4) | (second >> 4))],
        ));
        if chunk.len() > 1 {
            encoded.push(char::from(
                ALPHABET[usize::from(((second & 0b0000_1111) << 2) | (third >> 6))],
            ));
        } else {
            encoded.push('=');
        }
        if chunk.len() > 2 {
            encoded.push(char::from(ALPHABET[usize::from(third & 0b0011_1111)]));
        } else {
            encoded.push('=');
        }
    }
    encoded
}

/// Large content supplied directly to the existing Worker Artifact cache.
pub struct WorkerArtifactDraft<'content> {
    pub artifact_id: ArtifactId,
    pub kind: ArtifactKind,
    pub media_type: String,
    pub file_name: Option<String>,
    pub content: &'content [u8],
}

/// Fenced, content-addressed write passed to an existing Artifact cache adapter.
pub struct FencedArtifactWrite<'write> {
    pub identity: &'write RuntimeReplayIdentity,
    pub descriptor: &'write ArtifactDescriptor,
    pub content: &'write [u8],
}

/// Existing Worker Artifact cache seam; this module provides no storage implementation.
pub trait WorkerArtifactCache {
    type Error;

    /// Durably stores exact content under the supplied descriptor and authority.
    ///
    /// Implementations must treat exact retries idempotently and reject changed
    /// bytes, lease, fencing token, or source identity.
    ///
    /// # Errors
    ///
    /// Returns the existing cache adapter's validation or persistence error.
    fn store(&mut self, write: FencedArtifactWrite<'_>) -> Result<(), Self::Error>;
}

/// Artifact reference preparation failure.
#[derive(Debug, PartialEq, Eq)]
pub enum WorkerArtifactReferenceError<AuthorityError, CacheError> {
    InvalidArtifact,
    Authority(AuthorityError),
    Cache(CacheError),
}

impl<AuthorityError: fmt::Debug, CacheError: fmt::Debug> fmt::Display
    for WorkerArtifactReferenceError<AuthorityError, CacheError>
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidArtifact => formatter.write_str("Worker Artifact input is invalid"),
            Self::Authority(error) => {
                write!(formatter, "Worker Artifact authority failed: {error:?}")
            }
            Self::Cache(error) => write!(formatter, "Worker Artifact cache failed: {error:?}"),
        }
    }
}

impl<AuthorityError: fmt::Debug, CacheError: fmt::Debug> std::error::Error
    for WorkerArtifactReferenceError<AuthorityError, CacheError>
{
}

/// Writes large bytes to an injected fenced cache and returns only their reference.
///
/// Authority is checked before the cache adapter is called. The exact lease,
/// fencing token, and source identity are also carried into the cache write.
///
/// # Errors
///
/// Rejects malformed metadata before authority checks, rejects stale authority
/// before cache mutation, and forwards the existing cache adapter error.
pub fn persist_artifact_reference<A, Cache>(
    cache: &mut Cache,
    authority: &A,
    identity: &RuntimeReplayIdentity,
    draft: WorkerArtifactDraft<'_>,
) -> Result<ArtifactReference, WorkerArtifactReferenceError<A::Error, Cache::Error>>
where
    A: ReplayAuthority<Context = RuntimeReplayIdentity>,
    Cache: WorkerArtifactCache,
{
    if !canonical_id(&draft.artifact_id.0, "art_")
        || draft.media_type.is_empty()
        || draft.media_type.len() > 200
        || draft.media_type.chars().any(char::is_whitespace)
        || draft.file_name.as_ref().is_some_and(|name| {
            name.is_empty() || name.len() > 255 || name.chars().any(char::is_control)
        })
        || draft.content.len() > MAX_ARTIFACT_BYTES
    {
        return Err(WorkerArtifactReferenceError::InvalidArtifact);
    }
    let size_bytes = i64::try_from(draft.content.len())
        .map_err(|_| WorkerArtifactReferenceError::InvalidArtifact)?;
    let digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(draft.content)));
    let descriptor = ArtifactDescriptor {
        artifact_id: draft.artifact_id,
        digest: digest.clone(),
        file_name: draft.file_name,
        kind: draft.kind,
        media_type: draft.media_type,
        size_bytes,
    };
    authority
        .validate_active_lease(&identity.stream_key(), identity)
        .map_err(WorkerArtifactReferenceError::Authority)?;
    cache
        .store(FencedArtifactWrite {
            identity,
            descriptor: &descriptor,
            content: draft.content,
        })
        .map_err(WorkerArtifactReferenceError::Cache)?;
    Ok(ArtifactReference {
        artifact_id: descriptor.artifact_id,
        digest,
    })
}

/// Supplies strictly ordered identities for mandatory gateway decision events.
pub trait RuntimeTraceIdentitySource {
    type Error;

    /// Allocates the next runtime event and message identity.
    ///
    /// # Errors
    ///
    /// Returns the caller-owned durable sequence allocation failure.
    fn next_identity(&mut self) -> Result<RuntimeTraceIdentity, Self::Error>;
}

/// Durable Action Gateway journal backed by the existing Worker replay store.
pub struct RuntimeTraceActionJournal<Store, Authority, IdentitySource> {
    outbox: WorkerRuntimeTraceOutbox,
    store: Store,
    authority: Authority,
    identities: IdentitySource,
}

impl<Store, Authority, IdentitySource> RuntimeTraceActionJournal<Store, Authority, IdentitySource> {
    /// Binds the journal to caller-owned durable state and lease authority.
    #[must_use]
    pub const fn new(store: Store, authority: Authority, identities: IdentitySource) -> Self {
        Self {
            outbox: WorkerRuntimeTraceOutbox::new(),
            store,
            authority,
            identities,
        }
    }

    /// Returns caller-owned components for shutdown or Worker restart.
    #[must_use]
    pub fn into_parts(self) -> (Store, Authority, IdentitySource) {
        (self.store, self.authority, self.identities)
    }
}

/// Mandatory gateway-decision journal failure.
#[derive(Debug, PartialEq, Eq)]
pub enum RuntimeTraceActionJournalError<IdentityError, AuthorityError, StoreError> {
    Identity(IdentityError),
    Outbox(RuntimeTraceOutboxError<AuthorityError, StoreError>),
    NotDurablyReady,
}

impl<IdentityError: fmt::Debug, AuthorityError: fmt::Debug, StoreError: fmt::Debug> fmt::Display
    for RuntimeTraceActionJournalError<IdentityError, AuthorityError, StoreError>
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Identity(error) => write!(formatter, "runtime trace identity failed: {error:?}"),
            Self::Outbox(error) => error.fmt(formatter),
            Self::NotDurablyReady => {
                formatter.write_str("runtime trace decision is not durably ready")
            }
        }
    }
}

impl<IdentityError: fmt::Debug, AuthorityError: fmt::Debug, StoreError: fmt::Debug>
    std::error::Error
    for RuntimeTraceActionJournalError<IdentityError, AuthorityError, StoreError>
{
}

impl<Policy, Store, Authority, IdentitySource> PreActionDecisionRecorder<Policy>
    for RuntimeTraceActionJournal<Store, Authority, IdentitySource>
where
    Store: ReplayStore,
    Authority: ReplayAuthority<Context = RuntimeReplayIdentity>,
    IdentitySource: RuntimeTraceIdentitySource,
{
    type Error =
        RuntimeTraceActionJournalError<IdentitySource::Error, Authority::Error, Store::Error>;

    fn record(
        &mut self,
        input: GateInput<'_, Policy>,
        decision: &GateDecision,
    ) -> Result<(), Self::Error> {
        let action_identity = self
            .identities
            .next_identity()
            .map_err(RuntimeTraceActionJournalError::Identity)?;
        let action =
            RuntimeTraceDraft::gateway_action(action_identity, &input).map_err(|error| {
                RuntimeTraceActionJournalError::Outbox(RuntimeTraceOutboxError::Input(error))
            })?;
        require_durably_ready(
            &self
                .outbox
                .retain(&mut self.store, &self.authority, action)
                .map_err(RuntimeTraceActionJournalError::Outbox)?,
        )?;

        let gate_identity = self
            .identities
            .next_identity()
            .map_err(RuntimeTraceActionJournalError::Identity)?;
        let gate = RuntimeTraceDraft::gateway_decision(gate_identity, &input, decision).map_err(
            |error| RuntimeTraceActionJournalError::Outbox(RuntimeTraceOutboxError::Input(error)),
        )?;
        require_durably_ready(
            &self
                .outbox
                .retain(&mut self.store, &self.authority, gate)
                .map_err(RuntimeTraceActionJournalError::Outbox)?,
        )
    }
}

fn require_durably_ready<IdentityError, AuthorityError, StoreError>(
    retention: &RuntimeTraceRetention,
) -> Result<(), RuntimeTraceActionJournalError<IdentityError, AuthorityError, StoreError>> {
    match retention {
        RuntimeTraceRetention::Ready { .. } => Ok(()),
        RuntimeTraceRetention::Gap { .. } | RuntimeTraceRetention::Conflict { .. } => {
            Err(RuntimeTraceActionJournalError::NotDurablyReady)
        }
    }
}

/// Returns the envelope token retained by a gateway trace fact.
#[must_use]
pub fn traced_envelope_token(fact: &RuntimeTraceFact) -> Option<ExecutionEnvelopeToken> {
    match fact {
        RuntimeTraceFact::Gate {
            envelope_version,
            envelope_digest,
            ..
        } => Some(ExecutionEnvelopeToken {
            version: *envelope_version,
            digest: envelope_digest.clone(),
        }),
        _ => None,
    }
}
