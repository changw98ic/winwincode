// SPDX-License-Identifier: Apache-2.0

//! The canonical ten-object Delivery aggregate.
//!
//! The public structs in the child modules are draft/snapshot facts. A caller
//! cannot persist one until [`Delivery::try_from_snapshot`] has validated the
//! whole aggregate. `Delivery` exposes no mutable access, so every value that
//! crosses the command or store seam keeps the aggregate invariants.

mod attention;
pub mod candidate;
pub mod evidence;
pub mod rework;
mod session_binding;
mod spec;
mod stage_run;
mod task;
pub mod verdict;
pub mod verification;

use std::{
    collections::{HashMap, HashSet},
    error::Error,
    fmt,
};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub use attention::{AttentionItem, AttentionItemStatus, AttentionItemType, AttentionOption};
pub(crate) use candidate::assert_frozen_candidate_current;
pub use candidate::{
    CandidatePathFact, CandidatePathState, FreezeCandidateFacts, FrozenDeliveryCandidate,
    ValidatedGitSnapshotFact, freeze_delivery_candidate,
};
pub use evidence::{EvidenceRef, EvidenceRefType};
pub use session_binding::SessionBinding;
pub use spec::{
    AcceptanceCriterion, DeliveryPublicationTarget, DeliverySourceRef, DeliverySpec,
    GitHubIssueSourceRef, GitHubPullRequestTargetRef, RepositoryKind, RepositoryRef,
};
pub use stage_run::{DeliveryStage, StageRun, StageRunActorType, StageRunStatus};
pub use task::{DeliveryTask, DeliveryTaskStatus};
pub use verdict::{
    ComputedDeliveryVerdict, CriterionResult, CriterionVerdict, DeliveryVerdict,
    DeliveryVerdictStatus, compute_delivery_verdict,
};
pub use winwincode_domain::{AttentionItemId, DeliveryId, DeliveryTaskId, EvidenceId, StageRunId};

pub const DELIVERY_SCHEMA_VERSION: u8 = 3;
pub const MAX_DELIVERY_REWORK_ATTEMPTS: u64 = 100;
pub(crate) const MAX_TEXT_LENGTH: usize = 65_536;
pub(crate) const MAX_REFERENCE_LENGTH: usize = 4_096;
pub(crate) const MAX_COLLECTION_LENGTH: usize = 1_000;
pub(crate) const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

macro_rules! domain_identifier {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            /// Builds a validated legacy Delivery identifier.
            ///
            /// # Errors
            ///
            /// Returns [`DeliveryValidationError`] when the value is not a
            /// portable identifier accepted by the current Delivery records.
            pub fn new(value: impl Into<String>) -> Result<Self, DeliveryValidationError> {
                let value = value.into();
                portable_identifier(&value, stringify!($name))?;
                Ok(Self(value))
            }
        }
    };
}

domain_identifier!(DeliverySpecId);
domain_identifier!(AcceptanceCriterionId);
domain_identifier!(SessionBindingId);
domain_identifier!(CriterionResultId);
domain_identifier!(DeliveryVerdictId);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryValidationErrorCode {
    InvalidShape,
    UnsupportedSchemaVersion,
    InvalidIdentifier,
    InvalidValue,
    DuplicateId,
    RelationshipMismatch,
    InvalidVerdict,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryValidationError {
    code: DeliveryValidationErrorCode,
    path: String,
    message: String,
}

impl DeliveryValidationError {
    pub fn code(&self) -> DeliveryValidationErrorCode {
        self.code
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for DeliveryValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.path, self.message)
    }
}

impl Error for DeliveryValidationError {}

pub(crate) fn validation_error(
    code: DeliveryValidationErrorCode,
    path: impl Into<String>,
    message: impl Into<String>,
) -> DeliveryValidationError {
    DeliveryValidationError {
        code,
        path: path.into(),
        message: message.into(),
    }
}

pub(crate) fn deserialize_required_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

pub(crate) fn schema_version(value: u8, path: &str) -> Result<(), DeliveryValidationError> {
    if value == DELIVERY_SCHEMA_VERSION {
        Ok(())
    } else {
        Err(validation_error(
            DeliveryValidationErrorCode::UnsupportedSchemaVersion,
            path,
            format!("must be {DELIVERY_SCHEMA_VERSION}"),
        ))
    }
}

pub(crate) fn portable_identifier(value: &str, path: &str) -> Result<(), DeliveryValidationError> {
    let mut bytes = value.bytes();
    let first = bytes.next();
    let valid = value.len() <= 200
        && first.is_some_and(|byte| byte.is_ascii_alphanumeric())
        && bytes.all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'/' | b'@' | b'-')
        });
    if valid {
        Ok(())
    } else {
        Err(validation_error(
            DeliveryValidationErrorCode::InvalidIdentifier,
            path,
            "must be a portable identifier of at most 200 characters",
        ))
    }
}

pub(crate) fn request_identifier(value: &str, path: &str) -> Result<(), DeliveryValidationError> {
    let mut bytes = value.bytes();
    let first = bytes.next();
    let valid = value.len() <= 500
        && first.is_some_and(|byte| byte.is_ascii_alphanumeric())
        && bytes.all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'/' | b'@' | b'-')
        });
    if valid {
        Ok(())
    } else {
        Err(validation_error(
            DeliveryValidationErrorCode::InvalidIdentifier,
            path,
            "must be a portable request identifier of at most 500 characters",
        ))
    }
}

pub(crate) fn bounded_text(
    value: &str,
    path: &str,
    maximum: usize,
) -> Result<(), DeliveryValidationError> {
    let invalid_control = value.chars().any(|character| {
        let code = u32::from(character);
        matches!(code, 0..=8 | 11..=12 | 14..=31 | 127)
    });
    if !value.trim().is_empty() && value.encode_utf16().count() <= maximum && !invalid_control {
        Ok(())
    } else {
        Err(validation_error(
            DeliveryValidationErrorCode::InvalidValue,
            path,
            "must be non-empty bounded text",
        ))
    }
}

pub(crate) fn nullable_text(
    value: Option<&str>,
    path: &str,
    maximum: usize,
) -> Result<(), DeliveryValidationError> {
    value.map_or(Ok(()), |text| bounded_text(text, path, maximum))
}

pub(crate) fn safe_non_negative(value: u64, path: &str) -> Result<(), DeliveryValidationError> {
    if value <= MAX_SAFE_INTEGER {
        Ok(())
    } else {
        Err(validation_error(
            DeliveryValidationErrorCode::InvalidValue,
            path,
            "must be a non-negative safe integer",
        ))
    }
}

pub(crate) fn positive(value: u64, path: &str) -> Result<(), DeliveryValidationError> {
    safe_non_negative(value, path)?;
    if value > 0 {
        Ok(())
    } else {
        Err(validation_error(
            DeliveryValidationErrorCode::InvalidValue,
            path,
            "must be positive",
        ))
    }
}

pub(crate) fn collection_length(length: usize, path: &str) -> Result<(), DeliveryValidationError> {
    if length <= MAX_COLLECTION_LENGTH {
        Ok(())
    } else {
        Err(validation_error(
            DeliveryValidationErrorCode::InvalidValue,
            path,
            format!("must contain at most {MAX_COLLECTION_LENGTH} entries"),
        ))
    }
}

pub(crate) fn unique_texts(values: &[String], path: &str) -> Result<(), DeliveryValidationError> {
    collection_length(values.len(), path)?;
    let mut unique = HashSet::with_capacity(values.len());
    for (index, value) in values.iter().enumerate() {
        bounded_text(value, &format!("{path}[{index}]"), MAX_TEXT_LENGTH)?;
        if !unique.insert(value.as_str()) {
            return Err(validation_error(
                DeliveryValidationErrorCode::DuplicateId,
                path,
                "contains duplicate entries",
            ));
        }
    }
    Ok(())
}

pub(crate) fn duplicate_ids<'a>(
    ids: impl IntoIterator<Item = &'a str>,
    path: &str,
) -> Result<(), DeliveryValidationError> {
    let mut unique = HashSet::new();
    for id in ids {
        if !unique.insert(id) {
            return Err(validation_error(
                DeliveryValidationErrorCode::DuplicateId,
                path,
                "contains duplicate identities",
            ));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeliveryStatus {
    #[serde(rename = "draft")]
    Draft,
    #[serde(rename = "clarifying")]
    Clarifying,
    #[serde(rename = "ready")]
    Ready,
    #[serde(rename = "planning")]
    Planning,
    #[serde(rename = "plan-review")]
    PlanReview,
    #[serde(rename = "executing")]
    Executing,
    #[serde(rename = "verifying")]
    Verifying,
    #[serde(rename = "reworking")]
    Reworking,
    #[serde(rename = "needs-attention")]
    NeedsAttention,
    #[serde(rename = "ready-to-deliver")]
    ReadyToDeliver,
    #[serde(rename = "delivered")]
    Delivered,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeliverySnapshot {
    pub schema_version: u8,
    pub id: DeliveryId,
    pub revision: u64,
    pub status: DeliveryStatus,
    pub spec: DeliverySpec,
    pub tasks: Vec<DeliveryTask>,
    pub stage_runs: Vec<StageRun>,
    pub session_bindings: Vec<SessionBinding>,
    pub attention_items: Vec<AttentionItem>,
    pub evidence: Vec<EvidenceRef>,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub verdict: Option<DeliveryVerdict>,
    pub created_at_millis: u64,
    pub updated_at_millis: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    snapshot: DeliverySnapshot,
}

impl Delivery {
    /// Validates a complete snapshot and freezes it behind the aggregate.
    ///
    /// # Errors
    ///
    /// Returns [`DeliveryValidationError`] for any invalid fact or broken
    /// relationship across the ten canonical objects.
    pub fn try_from_snapshot(
        mut snapshot: DeliverySnapshot,
    ) -> Result<Self, DeliveryValidationError> {
        validate_delivery(&mut snapshot)?;
        Ok(Self { snapshot })
    }

    /// Decodes and validates one strict canonical snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`DeliveryValidationError`] for malformed JSON, missing or
    /// extra fields, invalid values, or broken aggregate relationships.
    pub fn decode_json(bytes: &[u8]) -> Result<Self, DeliveryValidationError> {
        let snapshot = serde_json::from_slice(bytes).map_err(|error| {
            validation_error(
                DeliveryValidationErrorCode::InvalidShape,
                "delivery",
                error.to_string(),
            )
        })?;
        Self::try_from_snapshot(snapshot)
    }

    /// Encodes the validated canonical snapshot.
    ///
    /// # Errors
    ///
    /// Returns a serialization error if the JSON encoder cannot represent the
    /// snapshot.
    pub fn encode_json(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(&self.snapshot)
    }

    pub fn snapshot(&self) -> &DeliverySnapshot {
        &self.snapshot
    }

    pub fn into_snapshot(self) -> DeliverySnapshot {
        self.snapshot
    }

    pub fn id(&self) -> &DeliveryId {
        &self.snapshot.id
    }

    pub fn revision(&self) -> u64 {
        self.snapshot.revision
    }
}

impl Serialize for Delivery {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.snapshot.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Delivery {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let snapshot = DeliverySnapshot::deserialize(deserializer)?;
        Self::try_from_snapshot(snapshot).map_err(serde::de::Error::custom)
    }
}

#[allow(clippy::too_many_lines)]
fn validate_delivery(snapshot: &mut DeliverySnapshot) -> Result<(), DeliveryValidationError> {
    schema_version(snapshot.schema_version, "delivery.schemaVersion")?;
    portable_identifier(&snapshot.id.0, "delivery.id")?;
    positive(snapshot.revision, "delivery.revision")?;
    safe_non_negative(snapshot.created_at_millis, "delivery.createdAtMillis")?;
    safe_non_negative(snapshot.updated_at_millis, "delivery.updatedAtMillis")?;
    if snapshot.updated_at_millis < snapshot.created_at_millis {
        return Err(validation_error(
            DeliveryValidationErrorCode::InvalidValue,
            "delivery.updatedAtMillis",
            "delivery update precedes creation",
        ));
    }

    spec::validate(&mut snapshot.spec, "delivery.spec")?;
    collection_length(snapshot.tasks.len(), "delivery.tasks")?;
    collection_length(snapshot.stage_runs.len(), "delivery.stageRuns")?;
    collection_length(snapshot.session_bindings.len(), "delivery.sessionBindings")?;
    collection_length(snapshot.attention_items.len(), "delivery.attentionItems")?;
    collection_length(snapshot.evidence.len(), "delivery.evidence")?;
    for (index, task) in snapshot.tasks.iter().enumerate() {
        task::validate(task, &format!("delivery.tasks[{index}]"))?;
    }
    for (index, run) in snapshot.stage_runs.iter().enumerate() {
        stage_run::validate(run, &format!("delivery.stageRuns[{index}]"))?;
    }
    for (index, binding) in snapshot.session_bindings.iter().enumerate() {
        session_binding::validate(binding, &format!("delivery.sessionBindings[{index}]"))?;
    }
    for (index, item) in snapshot.attention_items.iter().enumerate() {
        attention::validate(item, &format!("delivery.attentionItems[{index}]"))?;
    }
    for (index, evidence) in snapshot.evidence.iter().enumerate() {
        evidence::validate(evidence, &format!("delivery.evidence[{index}]"))?;
    }
    if let Some(verdict) = &snapshot.verdict {
        verdict::validate(verdict, "delivery.verdict")?;
    }

    if snapshot.spec.delivery_id != snapshot.id {
        return Err(validation_error(
            DeliveryValidationErrorCode::RelationshipMismatch,
            "delivery.spec.deliveryId",
            "spec belongs to another delivery",
        ));
    }
    duplicate_ids(
        snapshot.tasks.iter().map(|task| task.id.0.as_str()),
        "delivery.tasks",
    )?;
    duplicate_ids(
        snapshot.stage_runs.iter().map(|run| run.id.0.as_str()),
        "delivery.stageRuns",
    )?;
    duplicate_ids(
        snapshot
            .session_bindings
            .iter()
            .map(|binding| binding.id.0.as_str()),
        "delivery.sessionBindings",
    )?;
    duplicate_ids(
        snapshot
            .session_bindings
            .iter()
            .map(|binding| binding.execution_job_id.0.as_str()),
        "delivery.sessionBindings.executionJobId",
    )?;
    duplicate_ids(
        snapshot
            .session_bindings
            .iter()
            .filter_map(|binding| binding.worker_session_id.as_ref())
            .map(|session_id| session_id.0.as_str()),
        "delivery.sessionBindings.workerSessionId",
    )?;
    duplicate_ids(
        snapshot
            .session_bindings
            .iter()
            .filter_map(|binding| binding.codex_thread_id.as_ref())
            .map(|thread_id| thread_id.0.as_str()),
        "delivery.sessionBindings.codexThreadId",
    )?;
    duplicate_ids(
        snapshot
            .attention_items
            .iter()
            .map(|item| item.id.0.as_str()),
        "delivery.attentionItems",
    )?;
    duplicate_ids(
        snapshot
            .evidence
            .iter()
            .map(|reference| reference.id.0.as_str()),
        "delivery.evidence",
    )?;

    let criterion_ids: HashSet<&str> = snapshot
        .spec
        .acceptance_criteria
        .iter()
        .map(|criterion| criterion.id.0.as_str())
        .collect();
    let task_ids: HashSet<&str> = snapshot
        .tasks
        .iter()
        .map(|task| task.id.0.as_str())
        .collect();
    let runs_by_id: HashMap<&str, &StageRun> = snapshot
        .stage_runs
        .iter()
        .map(|run| (run.id.0.as_str(), run))
        .collect();
    let bindings_by_id: HashMap<&str, &SessionBinding> = snapshot
        .session_bindings
        .iter()
        .map(|binding| (binding.id.0.as_str(), binding))
        .collect();
    let evidence_by_id: HashMap<&str, &EvidenceRef> = snapshot
        .evidence
        .iter()
        .map(|reference| (reference.id.0.as_str(), reference))
        .collect();

    for (index, task) in snapshot.tasks.iter().enumerate() {
        if task.delivery_id != snapshot.id
            || task
                .acceptance_criterion_ids
                .iter()
                .any(|criterion_id| !criterion_ids.contains(criterion_id.0.as_str()))
        {
            return Err(validation_error(
                DeliveryValidationErrorCode::RelationshipMismatch,
                format!("delivery.tasks[{index}]"),
                "delivery task does not match the delivery or its acceptance criteria",
            ));
        }
    }
    task::validate_graph(&snapshot.tasks, "delivery.tasks")?;

    for (index, run) in snapshot.stage_runs.iter().enumerate() {
        if run.delivery_id != snapshot.id
            || run
                .delivery_task_id
                .as_ref()
                .is_some_and(|task_id| !task_ids.contains(task_id.0.as_str()))
        {
            return Err(validation_error(
                DeliveryValidationErrorCode::RelationshipMismatch,
                format!("delivery.stageRuns[{index}]"),
                "stage run does not match the delivery or a delivery task",
            ));
        }
        if run.stage == DeliveryStage::Reworking
            && (run.actor_type != StageRunActorType::Codex || run.role != "remediator")
        {
            return Err(validation_error(
                DeliveryValidationErrorCode::RelationshipMismatch,
                format!("delivery.stageRuns[{index}]"),
                "rework stage run must use a Codex remediator",
            ));
        }
    }
    let rework_count = snapshot
        .stage_runs
        .iter()
        .filter(|run| run.stage == DeliveryStage::Reworking)
        .count() as u64;
    if rework_count > snapshot.spec.max_rework_attempts {
        return Err(validation_error(
            DeliveryValidationErrorCode::RelationshipMismatch,
            "delivery.stageRuns",
            "delivery exceeds the approved rework attempt limit",
        ));
    }

    for (index, binding) in snapshot.session_bindings.iter().enumerate() {
        let run = runs_by_id.get(binding.stage_run_id.0.as_str());
        if binding.delivery_id != snapshot.id
            || run.is_none()
            || run.is_some_and(|run| {
                run.actor_type != StageRunActorType::Codex
                    || run.delivery_task_id != binding.delivery_task_id
                    || binding.bound_at_millis < run.started_at_millis
            })
        {
            return Err(validation_error(
                DeliveryValidationErrorCode::RelationshipMismatch,
                format!("delivery.sessionBindings[{index}]"),
                "session binding does not exactly match its Delivery, task, Codex StageRun, or start time",
            ));
        }
    }

    for (index, item) in snapshot.attention_items.iter().enumerate() {
        if item.delivery_id != snapshot.id
            || item.delivery_spec_id != snapshot.spec.id
            || item
                .stage_run_id
                .as_ref()
                .is_some_and(|run_id| !runs_by_id.contains_key(run_id.0.as_str()))
        {
            return Err(validation_error(
                DeliveryValidationErrorCode::RelationshipMismatch,
                format!("delivery.attentionItems[{index}]"),
                "attention item does not match its delivery, current spec, or stage run",
            ));
        }
    }

    for (index, reference) in snapshot.evidence.iter().enumerate() {
        let run = runs_by_id.get(reference.stage_run_id.0.as_str());
        let binding = bindings_by_id.get(reference.session_binding_id.0.as_str());
        let valid = reference.delivery_id == snapshot.id
            && reference.delivery_spec_id == snapshot.spec.id
            && reference.delivery_spec_revision == snapshot.spec.revision
            && run.is_some()
            && binding.is_some()
            && binding.is_some_and(|binding| {
                binding.delivery_id == snapshot.id
                    && binding.stage_run_id == reference.stage_run_id
                    && reference.created_at_millis >= binding.bound_at_millis
            })
            && run.is_some_and(|run| reference.created_at_millis >= run.started_at_millis);
        if !valid {
            return Err(validation_error(
                DeliveryValidationErrorCode::RelationshipMismatch,
                format!("delivery.evidence[{index}]"),
                "evidence does not match its delivery, current spec revision, stage run, session binding, or binding time",
            ));
        }
    }

    if snapshot.status == DeliveryStatus::NeedsAttention
        && !snapshot
            .attention_items
            .iter()
            .any(|item| item.blocking && item.status == AttentionItemStatus::Open)
    {
        return Err(validation_error(
            DeliveryValidationErrorCode::RelationshipMismatch,
            "delivery.attentionItems",
            "needs-attention delivery has no open blocking attention item",
        ));
    }

    if let Some(verdict) = &snapshot.verdict {
        validate_verdict_relationships(snapshot, verdict, &criterion_ids, &evidence_by_id)?;
    }

    if matches!(
        snapshot.status,
        DeliveryStatus::ReadyToDeliver | DeliveryStatus::Delivered
    ) && snapshot.verdict.as_ref().map(|verdict| verdict.status)
        != Some(DeliveryVerdictStatus::Pass)
    {
        return Err(validation_error(
            DeliveryValidationErrorCode::InvalidVerdict,
            "delivery.status",
            "ready-to-deliver and delivered require a passing delivery verdict",
        ));
    }
    if matches!(
        snapshot.status,
        DeliveryStatus::ReadyToDeliver | DeliveryStatus::Delivered
    ) && snapshot
        .tasks
        .iter()
        .any(|task| task.status != DeliveryTaskStatus::Completed)
    {
        return Err(validation_error(
            DeliveryValidationErrorCode::RelationshipMismatch,
            "delivery.tasks",
            "ready-to-deliver and delivered require every DeliveryTask to be completed",
        ));
    }
    if snapshot.status == DeliveryStatus::Delivered
        && snapshot
            .attention_items
            .iter()
            .any(|item| item.blocking && item.status == AttentionItemStatus::Open)
    {
        return Err(validation_error(
            DeliveryValidationErrorCode::RelationshipMismatch,
            "delivery.attentionItems",
            "delivered delivery cannot retain blocking attention",
        ));
    }
    Ok(())
}

fn validate_verdict_relationships(
    snapshot: &DeliverySnapshot,
    verdict: &DeliveryVerdict,
    criterion_ids: &HashSet<&str>,
    evidence_by_id: &HashMap<&str, &EvidenceRef>,
) -> Result<(), DeliveryValidationError> {
    if verdict.delivery_id != snapshot.id || verdict.delivery_spec_id != snapshot.spec.id {
        return Err(validation_error(
            DeliveryValidationErrorCode::RelationshipMismatch,
            "delivery.verdict",
            "delivery verdict does not match the current delivery and spec",
        ));
    }
    let results_by_criterion: HashMap<&str, &CriterionResult> = verdict
        .criteria
        .iter()
        .map(|result| (result.criterion_id.0.as_str(), result))
        .collect();
    if results_by_criterion.len() != criterion_ids.len()
        || criterion_ids
            .iter()
            .any(|criterion_id| !results_by_criterion.contains_key(*criterion_id))
    {
        return Err(validation_error(
            DeliveryValidationErrorCode::InvalidVerdict,
            "delivery.verdict.criteria",
            "delivery verdict must evaluate every current acceptance criterion exactly once",
        ));
    }
    for (index, result) in verdict.criteria.iter().enumerate() {
        if result.delivery_id != snapshot.id
            || result.delivery_spec_id != snapshot.spec.id
            || result.candidate_ref != verdict.candidate_ref
            || result.evaluated_at_millis > verdict.produced_at_millis
        {
            return Err(validation_error(
                DeliveryValidationErrorCode::RelationshipMismatch,
                format!("delivery.verdict.criteria[{index}]"),
                "criterion result does not match the verdict identity or production time",
            ));
        }
        for evidence_id in &result.evidence_refs {
            let reference = evidence_by_id.get(evidence_id.0.as_str());
            if reference.is_none_or(|reference| {
                reference.candidate_ref != verdict.candidate_ref
                    || reference.created_at_millis > result.evaluated_at_millis
            }) {
                return Err(validation_error(
                    DeliveryValidationErrorCode::RelationshipMismatch,
                    format!("delivery.verdict.criteria[{index}].evidenceRefs"),
                    "criterion result cites missing, later, or foreign-candidate evidence",
                ));
            }
        }
    }

    let required_results = snapshot
        .spec
        .acceptance_criteria
        .iter()
        .filter(|criterion| criterion.required)
        .filter_map(|criterion| results_by_criterion.get(criterion.id.0.as_str()).copied());
    let required_results: Vec<_> = required_results.collect();
    let expected = if required_results
        .iter()
        .any(|result| result.verdict == CriterionVerdict::Fail)
    {
        DeliveryVerdictStatus::Fail
    } else if required_results
        .iter()
        .any(|result| result.verdict == CriterionVerdict::InfraError)
    {
        DeliveryVerdictStatus::InfraError
    } else if required_results
        .iter()
        .any(|result| result.verdict == CriterionVerdict::Inconclusive)
        || !verdict.unresolved_findings.is_empty()
    {
        DeliveryVerdictStatus::Inconclusive
    } else {
        DeliveryVerdictStatus::Pass
    };
    if verdict.status != expected {
        return Err(validation_error(
            DeliveryValidationErrorCode::InvalidVerdict,
            "delivery.verdict.status",
            format!("delivery verdict must be {expected:?} for its required criterion results"),
        ));
    }
    if verdict.status == DeliveryVerdictStatus::Pass
        && snapshot.attention_items.iter().any(|item| {
            item.blocking
                && item.status == AttentionItemStatus::Open
                && item.item_type != AttentionItemType::DeliveryApproval
        })
    {
        return Err(validation_error(
            DeliveryValidationErrorCode::InvalidVerdict,
            "delivery.verdict.status",
            "a passing verdict cannot retain open blocking Attention",
        ));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn test_fixture() -> DeliverySnapshot {
    serde_json::from_slice(include_bytes!("../../tests/fixtures/delivery-main.json"))
        .expect("checked Delivery fixture shape")
}

#[cfg(test)]
mod aggregate_tests {
    use super::{Delivery, DeliveryValidationErrorCode};

    #[test]
    fn canonical_delivery_rejects_missing_nullable_fields_and_extra_runtime_facts() {
        let mut value: serde_json::Value =
            serde_json::from_slice(include_bytes!("../../tests/fixtures/delivery-main.json"))
                .expect("fixture json");
        value["spec"]
            .as_object_mut()
            .expect("spec")
            .remove("sourceRef");
        assert_eq!(
            Delivery::decode_json(&serde_json::to_vec(&value).expect("json"))
                .expect_err("missing nullable field")
                .code(),
            DeliveryValidationErrorCode::InvalidShape
        );

        let mut value: serde_json::Value =
            serde_json::from_slice(include_bytes!("../../tests/fixtures/delivery-main.json"))
                .expect("fixture json");
        value["codexPlan"] = serde_json::json!([{ "step": "execution-owned" }]);
        assert_eq!(
            Delivery::decode_json(&serde_json::to_vec(&value).expect("json"))
                .expect_err("extra runtime fact")
                .code(),
            DeliveryValidationErrorCode::InvalidShape
        );
    }
}
