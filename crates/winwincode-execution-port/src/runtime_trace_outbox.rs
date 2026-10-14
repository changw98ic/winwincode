// SPDX-License-Identifier: Apache-2.0

use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_domain::{
    ArtifactId, ExecutionEventId, ExecutionMessageId, ExecutionSequence, Instant, SchemaVersion,
    SessionIdentity, Sha256Digest, WorkerSessionId,
};

use crate::action_gateway::{
    GateDecision, GateInput, PostActionHook, PostActionOutcome, PreActionDecisionRecorder,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretSafeTraceSummary(String);

impl SecretSafeTraceSummary {
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

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageTraceState {
    Started,
    Paused,
    Resumed,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolTraceOutcome {
    Succeeded,
    Failed,
    Cancelled,
}

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

pub use crate::generated::ExecutionMode;

impl ExecutionMode {
    #[must_use]
    pub fn from_config(value: &str) -> Option<Self> {
        match value {
            "react" => Some(Self::React),
            "delegated_patch_shadow" => Some(Self::DelegatedPatchShadow),
            "delegated_patch" => Some(Self::DelegatedPatch),
            "debug_probe" => Some(Self::DebugProbe),
            _ => None,
        }
    }

    #[must_use]
    pub const fn as_config(self) -> &'static str {
        match self {
            Self::React => "react",
            Self::DelegatedPatchShadow => "delegated_patch_shadow",
            Self::DelegatedPatch => "delegated_patch",
            Self::DebugProbe => "debug_probe",
        }
    }
}

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PerformanceBaselineReport {
    #[serde(default, skip_serializing_if = "usage_incomplete")]
    pub usage_complete: bool,
    pub execution_mode: ExecutionMode,
    pub observer_mode: ObserverMode,
    pub primary_model_call_count: i64,
    pub primary_model_input_tokens: Option<i64>,
    pub primary_model_cached_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_model_inclusive_input_tokens: Option<i64>,
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

#[allow(clippy::trivially_copy_pass_by_ref)]
fn usage_incomplete(value: &bool) -> bool {
    !value
}

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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IgnoredLegacyField;

impl<'de> Deserialize<'de> for IgnoredLegacyField {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        serde::de::IgnoredAny::deserialize(deserializer).map(|_| Self)
    }
}

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
        #[serde(default, skip_serializing, rename = "envelope_version")]
        legacy_envelope_version: IgnoredLegacyField,
        #[serde(default, skip_serializing, rename = "envelope_digest")]
        legacy_envelope_digest: IgnoredLegacyField,
    },
    Tool {
        source: ActionSource,
        outcome: ToolTraceOutcome,
    },
    Hook {
        source: ActionSource,
        operation: ActionOperation,
        outcome: ToolTraceOutcome,
        actions: Vec<PostActionHook>,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimeTracePayload {
    pub schema_version: String,
    pub fact: RuntimeTraceFact,
    pub artifacts: Vec<ArtifactReference>,
}

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

#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeTraceDraft {
    pub identity: RuntimeTraceIdentity,
    pub category: ExecutionEventCategory,
    pub summary: SecretSafeTraceSummary,
    pub fact: RuntimeTraceFact,
    pub artifacts: Vec<ArtifactReference>,
}

impl RuntimeTraceDraft {
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
                legacy_envelope_version: IgnoredLegacyField,
                legacy_envelope_digest: IgnoredLegacyField,
            },
            artifacts: Vec::new(),
        })
    }

    pub fn gateway_post_action<Policy>(
        identity: RuntimeTraceIdentity,
        input: &GateInput<'_, Policy>,
        outcome: PostActionOutcome,
        actions: Vec<PostActionHook>,
    ) -> Result<Self, RuntimeTraceInputError> {
        Self::post_action(
            identity,
            input.observed.source,
            input.observed.operation,
            outcome,
            actions,
        )
    }

    pub fn post_action(
        identity: RuntimeTraceIdentity,
        source: ActionSource,
        operation: ActionOperation,
        outcome: PostActionOutcome,
        actions: Vec<PostActionHook>,
    ) -> Result<Self, RuntimeTraceInputError> {
        let outcome = match outcome {
            PostActionOutcome::Succeeded => ToolTraceOutcome::Succeeded,
            PostActionOutcome::Failed => ToolTraceOutcome::Failed,
        };
        let summary = SecretSafeTraceSummary::new(format!(
            "post-action {outcome:?} for {source:?} {operation:?}"
        ))?;
        Ok(Self {
            identity,
            category: ExecutionEventCategory::Activity,
            summary,
            fact: RuntimeTraceFact::Hook {
                source,
                operation,
                outcome,
                actions,
            },
            artifacts: Vec::new(),
        })
    }
}

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

#[derive(Debug, Clone, PartialEq)]
pub enum RuntimeTraceRetention {
    Ready {
        message: Box<RuntimeEventMessage>,
        highest_sequence: u64,
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

#[derive(Debug, Clone, Copy, Default)]
pub struct WorkerRuntimeTraceOutbox {
    responder: RuntimeReplayResponder,
}

impl WorkerRuntimeTraceOutbox {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            responder: RuntimeReplayResponder::new(),
        }
    }

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

    /// Proves historical transport cleanup from an already acknowledged
    /// original frame without live authority lookup or cursor writes.
    ///
    /// # Errors
    ///
    /// Forwards malformed acknowledgement, original-frame and storage errors.
    pub fn retained_acknowledgement<S, A>(
        &self,
        store: &mut S,
        acknowledgement: &RuntimeAckMessage,
    ) -> Result<Option<RuntimeReplayAckReceipt>, RuntimeReplayError<A::Error, S::Error>>
    where
        S: ReplayAcknowledgementStore,
        A: ReplayAuthority<Context = RuntimeReplayIdentity>,
    {
        self.responder
            .retained_acknowledgement::<S, A>(store, acknowledgement)
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
        RuntimeTraceFact::Candidate { digest } if !sha256_digest(&digest.0) => {
            return Err(RuntimeTraceInputError::InvalidCandidateDigest);
        }
        RuntimeTraceFact::Hook { actions, .. } if actions.is_empty() => {
            return Err(RuntimeTraceInputError::InvalidIdentity);
        }
        RuntimeTraceFact::PerformanceBaseline { report }
            if performance_values(report)
                .any(|value| !(0..=MAX_SAFE_SEQUENCE).contains(&value))
                || !valid_cache_breakdown(report) =>
        {
            return Err(RuntimeTraceInputError::InvalidIdentity);
        }
        _ => {}
    }
    Ok(())
}

fn valid_cache_breakdown(report: &PerformanceBaselineReport) -> bool {
    let sum = match (
        report.primary_model_input_tokens,
        report.primary_model_cached_tokens,
    ) {
        (Some(input), Some(cached)) => input.checked_add(cached),
        (None, None) => report.primary_model_inclusive_input_tokens,
        _ => None,
    };
    sum.is_some_and(|sum| {
        (0..=MAX_SAFE_SEQUENCE).contains(&sum)
            && report
                .primary_model_inclusive_input_tokens
                .is_none_or(|inclusive| inclusive == sum)
    })
}

fn performance_values(report: &PerformanceBaselineReport) -> impl Iterator<Item = i64> {
    [
        report.primary_model_call_count,
        report.primary_model_input_tokens.unwrap_or(0),
        report.primary_model_cached_tokens.unwrap_or(0),
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

pub struct WorkerArtifactDraft<'content> {
    pub artifact_id: ArtifactId,
    pub kind: ArtifactKind,
    pub media_type: String,
    pub file_name: Option<String>,
    pub content: &'content [u8],
}

pub struct FencedArtifactWrite<'write> {
    pub identity: &'write RuntimeReplayIdentity,
    pub descriptor: &'write ArtifactDescriptor,
    pub content: &'write [u8],
}

pub trait WorkerArtifactCache {
    type Error;

    fn store(&mut self, write: FencedArtifactWrite<'_>) -> Result<(), Self::Error>;
}

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

pub trait RuntimeTraceIdentitySource {
    type Error;

    fn next_identity(&mut self) -> Result<RuntimeTraceIdentity, Self::Error>;
}

pub struct RuntimeTraceActionJournal<Store, Authority, IdentitySource> {
    outbox: WorkerRuntimeTraceOutbox,
    store: Store,
    authority: Authority,
    identities: IdentitySource,
}

impl<Store, Authority, IdentitySource> RuntimeTraceActionJournal<Store, Authority, IdentitySource> {
    #[must_use]
    pub const fn new(store: Store, authority: Authority, identities: IdentitySource) -> Self {
        Self {
            outbox: WorkerRuntimeTraceOutbox::new(),
            store,
            authority,
            identities,
        }
    }

    #[must_use]
    pub fn into_parts(self) -> (Store, Authority, IdentitySource) {
        (self.store, self.authority, self.identities)
    }
}

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

    fn record_post_action(
        &mut self,
        input: GateInput<'_, Policy>,
        outcome: PostActionOutcome,
        hooks: &[PostActionHook],
    ) -> Result<(), Self::Error> {
        if hooks.is_empty() {
            return Ok(());
        }
        let identity = self
            .identities
            .next_identity()
            .map_err(RuntimeTraceActionJournalError::Identity)?;
        let trace =
            RuntimeTraceDraft::gateway_post_action(identity, &input, outcome, hooks.to_vec())
                .map_err(|error| {
                    RuntimeTraceActionJournalError::Outbox(RuntimeTraceOutboxError::Input(error))
                })?;
        require_durably_ready(
            &self
                .outbox
                .retain(&mut self.store, &self.authority, trace)
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
