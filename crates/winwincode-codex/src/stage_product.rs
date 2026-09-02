// SPDX-License-Identifier: Apache-2.0

//! `StrongFlow` role policy and strict semantic stage-product preparation.
//!
//! This module deliberately stops before durable identity allocation. It
//! validates one already-authenticated [`ExecutionJob`], prepares the exact
//! Codex Core role policy, and converts a final Planner assistant message into
//! one canonical semantic product. The production adapter remains responsible
//! for assigning the lease-bound runtime event identity and retaining it in its
//! existing outbox before delivery.

use std::{collections::HashSet, fmt};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use winwincode_domain::{SchemaVersion, Sha256Digest};
use winwincode_execution_port::generated::{
    DeliveryStageInput, ExecutionEventCategory, ExecutionJob, ExecutionScope,
    ExecutionWorkspaceWriteMode,
};
use winwincode_kernel::RoleSessionPolicy;

/// Media type consumed by the production Planning-to-PlanReview authority.
pub const PLANNER_SOLUTION_MEDIA_TYPE: &str = "application/vnd.winwincode.planner-solution+json";

/// Protocol consumed by the production Solution Review authority.
pub const PLANNER_SOLUTION_PROTOCOL: &str = "winwincode.planner-solution.v1";

/// JSON media type consumed by the verification verdict authority.
pub const VERIFICATION_JSON_MEDIA_TYPE: &str = "application/json";

/// First read-only verification event protocol.
pub const VERIFICATION_SESSION_POLICY_PROTOCOL: &str = "winwincode.verification-session-policy.v1";

/// Final independent verification result protocol.
pub const VERIFICATION_RESULT_PROTOCOL: &str = "winwincode.independent-verification-result.v1";

const ROLE_POLICY_SCHEMA_VERSION: u32 = 1;
const PLANNER_SOLUTION_SCHEMA_VERSION: u8 = 1;
const MAX_PLANNER_SOLUTION_BYTES: usize = 1024 * 1024;
const MAX_STAGE_PROMPT_BYTES: usize = 1024 * 1024;

/// Stable stage-product failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StageProductErrorCode {
    InvalidJob,
    InvalidRole,
    InvalidScope,
    InvalidOutput,
    NonCanonicalOutput,
}

/// Seals every role- and workspace-relevant Job field used to open a Codex
/// session. The production adapter persists this digest beside the run and
/// requires an exact match before it reuses or resumes the session.
///
/// # Errors
///
/// Returns a bounded error if the generated Job cannot be canonically encoded.
pub fn stage_product_job_digest(job: &ExecutionJob) -> Result<Sha256Digest, StageProductError> {
    let bytes = serde_json::to_vec(job).map_err(|_| {
        StageProductError::new(
            StageProductErrorCode::InvalidJob,
            "ExecutionJob cannot be sealed for stage-product replay",
        )
    })?;
    Ok(Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes))))
}

/// Seals the immutable logical Job fields shared by replacement attempts.
///
/// # Errors
///
/// Returns a bounded error if the generated Job cannot be canonically encoded
/// or its required attempt field is absent from the generated representation.
pub fn stage_product_logical_job_digest(
    job: &ExecutionJob,
) -> Result<Sha256Digest, StageProductError> {
    let mut value = serde_json::to_value(job).map_err(|_| {
        StageProductError::new(
            StageProductErrorCode::InvalidJob,
            "ExecutionJob cannot be sealed for logical replacement replay",
        )
    })?;
    let object = value.as_object_mut().ok_or_else(|| {
        StageProductError::new(
            StageProductErrorCode::InvalidJob,
            "ExecutionJob logical replacement authority is not an object",
        )
    })?;
    if object.remove("attempt").is_none() {
        return Err(StageProductError::new(
            StageProductErrorCode::InvalidJob,
            "ExecutionJob logical replacement authority has no attempt",
        ));
    }
    let bytes = serde_json::to_vec(&value).map_err(|_| {
        StageProductError::new(
            StageProductErrorCode::InvalidJob,
            "ExecutionJob logical replacement authority cannot be encoded",
        )
    })?;
    Ok(Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes))))
}

/// Bounded failure which never retains the rejected model output.
#[derive(Debug, Eq, PartialEq)]
pub struct StageProductError {
    code: StageProductErrorCode,
    message: &'static str,
}

impl StageProductError {
    const fn new(code: StageProductErrorCode, message: &'static str) -> Self {
        Self { code, message }
    }

    /// Returns the stable machine-readable failure category.
    #[must_use]
    pub const fn code(&self) -> StageProductErrorCode {
        self.code
    }
}

impl fmt::Display for StageProductError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for StageProductError {}

/// Semantic payload ready for lease-bound runtime event identity allocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedStageProduct {
    category: ExecutionEventCategory,
    media_type: &'static str,
    bytes: Vec<u8>,
    digest: Sha256Digest,
    summary: &'static str,
}

impl PreparedStageProduct {
    #[must_use]
    pub const fn category(&self) -> &ExecutionEventCategory {
        &self.category
    }

    #[must_use]
    pub const fn media_type(&self) -> &'static str {
        self.media_type
    }

    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    #[must_use]
    pub const fn digest(&self) -> &Sha256Digest {
        &self.digest
    }

    #[must_use]
    pub const fn summary(&self) -> &'static str {
        self.summary
    }
}

/// Returns the canonical Codex Core role policy for one authenticated Job.
///
/// Product-session chat jobs intentionally return `None`. Delivery-stage jobs
/// must use one known `StrongFlow` role and cannot silently fall back to a
/// process-wide permission profile.
///
/// # Errors
///
/// Rejects a `StrongFlow` role on a `ProductSession` job or an unknown role on a
/// Delivery-stage job.
pub fn role_session_policy(
    job: &ExecutionJob,
) -> Result<Option<RoleSessionPolicy>, StageProductError> {
    match &job.scope {
        ExecutionScope::ProductSessionExecutionScope(_) => {
            if job.stage_input.is_some() || canonical_role(&job.execution_profile).is_some() {
                return Err(StageProductError::new(
                    StageProductErrorCode::InvalidScope,
                    "StrongFlow input or role requires a Delivery-stage execution scope",
                ));
            }
            Ok(None)
        }
        ExecutionScope::DeliveryStageExecutionScope(_) => {
            validate_stage_input(job)?;
            let role = canonical_role(&job.execution_profile).ok_or_else(|| {
                StageProductError::new(
                    StageProductErrorCode::InvalidRole,
                    "Delivery-stage execution profile is not a canonical StrongFlow role",
                )
            })?;
            let expected_write_mode = if matches!(role.workspace_mode, "candidate-write") {
                ExecutionWorkspaceWriteMode::Candidate
            } else {
                ExecutionWorkspaceWriteMode::ReadOnly
            };
            if job.workspace.write_mode != expected_write_mode {
                return Err(StageProductError::new(
                    StageProductErrorCode::InvalidScope,
                    "Delivery role and workspace write mode do not agree",
                ));
            }
            Ok(Some(RoleSessionPolicy {
                schema_version: ROLE_POLICY_SCHEMA_VERSION,
                role_id: job.execution_profile.clone(),
                workspace_mode: role.workspace_mode.to_owned(),
                developer_instructions: role.developer_instructions.to_owned(),
            }))
        }
    }
}

/// Builds the exact first-turn prompt from the sealed typed Job input.
///
/// `ProductSession` Chat keeps its original goal. Delivery-stage turns receive
/// the canonical `stageInput` JSON plus role-specific final-output rules, so a
/// restart never has to query mutable Delivery state or hide JSON in `goal`.
///
/// # Errors
///
/// Rejects a missing, oversized, role-incompatible or internally inconsistent
/// Delivery-stage input.
pub fn stage_product_prompt(job: &ExecutionJob) -> Result<String, StageProductError> {
    match &job.scope {
        ExecutionScope::ProductSessionExecutionScope(_) => {
            if job.stage_input.is_some() || canonical_role(&job.execution_profile).is_some() {
                return Err(StageProductError::new(
                    StageProductErrorCode::InvalidScope,
                    "ProductSession prompt cannot carry StrongFlow stage input",
                ));
            }
            Ok(job.goal.clone())
        }
        ExecutionScope::DeliveryStageExecutionScope(_) => {
            let input = validate_stage_input(job)?;
            let encoded = serde_json::to_string(input).map_err(|_| invalid_job())?;
            let final_rule = match job.execution_profile.as_str() {
                "planner" => concat!(
                    "Return only canonical JSON using protocol ",
                    "winwincode.planner-solution.v1 and schemaVersion 1. ",
                    "Every task proposal acceptanceCriterionIds value must come from stageInput."
                ),
                "executor" | "remediator" => concat!(
                    "Apply the requested source change in the assigned checkout. ",
                    "Do not invent an Artifact reference; the Worker freezes the real Git candidate."
                ),
                "reviewer" | "verifier" | "adversarial-verifier" => concat!(
                    "Use read-only commands against exactly stageInput.candidateRef. ",
                    "Return only canonical JSON using protocol ",
                    "winwincode.independent-verification-result.v1; use the exact spec, ",
                    "revision, candidate and criterion IDs from stageInput. ",
                    "Each evidence_sources entry must use the observed tool call ID as source_id ",
                    "for a direct Command or Test result; the Worker binds that source_id to its ",
                    "durable event."
                ),
                "requirements" | "solution" => {
                    "Use only the sealed Delivery specification and report the requested stage result."
                }
                _ => return Err(invalid_job()),
            };
            let prompt = format!(
                "Goal:\n{}\n\nStrongFlow stageInput (canonical JSON):\n{}\n\nRequired behavior:\n{}",
                job.goal, encoded, final_rule
            );
            if prompt.len() > MAX_STAGE_PROMPT_BYTES {
                return Err(StageProductError::new(
                    StageProductErrorCode::InvalidJob,
                    "StrongFlow stage prompt exceeds the supported size",
                ));
            }
            Ok(prompt)
        }
    }
}

fn validate_stage_input(job: &ExecutionJob) -> Result<&DeliveryStageInput, StageProductError> {
    let ExecutionScope::DeliveryStageExecutionScope(scope) = &job.scope else {
        return Err(StageProductError::new(
            StageProductErrorCode::InvalidScope,
            "Delivery stage input requires a Delivery-stage scope",
        ));
    };
    let input = job.stage_input.as_ref().ok_or_else(invalid_job)?;
    let criterion_ids = criterion_ids(input);
    let valid = common_stage_input(input)
        && valid_criteria(input, &criterion_ids)
        && valid_task(input, scope.delivery_task_id.as_ref(), &criterion_ids)
        && valid_role_shape(job, input, scope);
    valid.then_some(input).ok_or_else(invalid_job)
}

fn criterion_ids(input: &DeliveryStageInput) -> HashSet<&str> {
    input
        .acceptance_criteria
        .iter()
        .map(|criterion| criterion.criterion_id.as_str())
        .collect()
}

fn valid_criteria(input: &DeliveryStageInput, criterion_ids: &HashSet<&str>) -> bool {
    !input.acceptance_criteria.is_empty()
        && input.acceptance_criteria.len() <= 1_000
        && criterion_ids.len() == input.acceptance_criteria.len()
        && input
            .acceptance_criteria
            .iter()
            .any(|criterion| criterion.required)
        && input.acceptance_criteria.iter().all(|criterion| {
            portable_identity(&criterion.criterion_id)
                && bounded(&criterion.description, 1, 20_000)
                && criterion
                    .verification_method
                    .as_deref()
                    .is_none_or(|method| bounded(method, 1, 20_000))
        })
}

fn valid_task(
    input: &DeliveryStageInput,
    scoped_task_id: Option<&winwincode_domain::DeliveryTaskId>,
    criterion_ids: &HashSet<&str>,
) -> bool {
    input.task.as_ref().is_none_or(|task| {
        scoped_task_id == Some(&task.task_id)
            && bounded(&task.title, 1, 256)
            && bounded(&task.goal, 1, 20_000)
            && !task.acceptance_criterion_ids.is_empty()
            && task.acceptance_criterion_ids.len() <= 1_000
            && task
                .acceptance_criterion_ids
                .iter()
                .collect::<HashSet<_>>()
                .len()
                == task.acceptance_criterion_ids.len()
            && task
                .acceptance_criterion_ids
                .iter()
                .all(|criterion| criterion_ids.contains(criterion.as_str()))
    })
}

fn valid_role_shape(
    job: &ExecutionJob,
    input: &DeliveryStageInput,
    scope: &winwincode_execution_port::generated::DeliveryStageExecutionScope,
) -> bool {
    match job.execution_profile.as_str() {
        "requirements" | "solution" | "planner" => {
            input.task.is_none()
                && scope.delivery_task_id.is_none()
                && input.candidate_ref.is_none()
                && job.goal == input.goal
        }
        "executor" => {
            input.task.is_some()
                && input.candidate_ref.is_none()
                && input
                    .task
                    .as_ref()
                    .is_some_and(|task| job.goal == task.goal)
        }
        "reviewer" | "verifier" | "adversarial-verifier" => {
            input.task.is_some()
                && input
                    .candidate_ref
                    .as_deref()
                    .is_some_and(valid_candidate_ref)
                && input
                    .task
                    .as_ref()
                    .is_some_and(|task| job.goal == task.goal)
        }
        "remediator" => {
            input.task.is_some()
                && input
                    .candidate_ref
                    .as_deref()
                    .is_some_and(valid_candidate_ref)
                && input.candidate_ref.as_ref()
                    == scope
                        .rework_authorization
                        .as_ref()
                        .map(|authorization| &authorization.candidate_ref)
                && input
                    .task
                    .as_ref()
                    .is_some_and(|task| job.goal == task.goal)
        }
        _ => false,
    }
}

fn common_stage_input(input: &DeliveryStageInput) -> bool {
    input.schema_version == SchemaVersion::WinwincodeV1
        && portable_identity(&input.delivery_spec_id)
        && (1..=9_007_199_254_740_991).contains(&input.delivery_spec_revision)
        && bounded(&input.title, 1, 256)
        && bounded(&input.goal, 1, 20_000)
        && unique_texts(&input.scope, true)
        && unique_texts(&input.out_of_scope, false)
        && unique_texts(&input.constraints, false)
}

fn invalid_job() -> StageProductError {
    StageProductError::new(
        StageProductErrorCode::InvalidJob,
        "ExecutionJob has invalid StrongFlow stage input",
    )
}

fn bounded(value: &str, minimum: usize, maximum: usize) -> bool {
    (minimum..=maximum).contains(&value.len()) && !value.chars().any(char::is_control)
}

fn portable_identity(value: &str) -> bool {
    let mut bytes = value.bytes();
    value.len() <= 200
        && bytes
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && bytes.all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'/' | b'@' | b'-')
        })
}

fn unique_texts(values: &[String], required: bool) -> bool {
    (!required || !values.is_empty())
        && values.len() <= 1_000
        && values.iter().all(|value| bounded(value, 1, 20_000))
        && values.iter().collect::<HashSet<_>>().len() == values.len()
}

/// Converts the Planner's exact final assistant message into the one semantic
/// Activity consumed by `planning_solution_authority`.
///
/// Durable event identity, sequence, occurrence time, lease, session and
/// Worker authority are intentionally absent. The Codex outbox owns those
/// facts and must add them only after this semantic payload succeeds.
///
/// # Errors
///
/// Rejects non-Planner jobs, malformed or oversized JSON, unknown fields,
/// another schema/protocol, and any byte representation which does not equal
/// the canonical serializer output.
pub fn prepare_planner_solution_activity(
    job: &ExecutionJob,
    final_message: &[u8],
) -> Result<PreparedStageProduct, StageProductError> {
    let Some(policy) = role_session_policy(job)? else {
        return Err(StageProductError::new(
            StageProductErrorCode::InvalidScope,
            "Planner product requires a Delivery-stage execution scope",
        ));
    };
    if policy.role_id != "planner" {
        return Err(StageProductError::new(
            StageProductErrorCode::InvalidRole,
            "Planner product requires the planner execution profile",
        ));
    }
    if final_message.is_empty() || final_message.len() > MAX_PLANNER_SOLUTION_BYTES {
        return Err(invalid_output());
    }
    let product: PlannerSolutionV1 =
        serde_json::from_slice(final_message).map_err(|_| invalid_output())?;
    if product.schema_version != PLANNER_SOLUTION_SCHEMA_VERSION
        || product.protocol != PLANNER_SOLUTION_PROTOCOL
        || !product.has_required_content()
        || !planner_matches_stage_input(&product, validate_stage_input(job)?)
    {
        return Err(invalid_output());
    }
    let canonical = serde_json::to_vec(&product).map_err(|_| invalid_output())?;
    if canonical != final_message {
        return Err(StageProductError::new(
            StageProductErrorCode::NonCanonicalOutput,
            "Planner final response is not canonical JSON",
        ));
    }
    let digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(&canonical)));
    Ok(PreparedStageProduct {
        category: ExecutionEventCategory::Activity,
        media_type: PLANNER_SOLUTION_MEDIA_TYPE,
        bytes: canonical,
        digest,
        summary: "planner produced a canonical Solution Review result",
    })
}

/// Prepares the first verification Lifecycle product for the exact frozen
/// candidate. The adapter emits this before command evidence or a final result.
///
/// # Errors
///
/// Rejects non-verification roles and malformed candidate references.
pub fn prepare_verification_policy_attestation(
    job: &ExecutionJob,
    candidate_ref: &str,
) -> Result<PreparedStageProduct, StageProductError> {
    ensure_verification_role(job)?;
    let input = validate_stage_input(job)?;
    if !valid_candidate_ref(candidate_ref) || input.candidate_ref.as_deref() != Some(candidate_ref)
    {
        return Err(invalid_output());
    }
    prepare_json_product(
        ExecutionEventCategory::Lifecycle,
        &VerificationPolicyAttestation {
            protocol: VERIFICATION_SESSION_POLICY_PROTOCOL,
            workspace_mode: "candidate-read-only",
            permission_profile: "candidate-read-only-restricted",
            candidate_ref,
        },
        "verification session attested the exact read-only candidate",
    )
}

/// Stable command outcome retained as direct verification evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum VerificationEvidenceStatus {
    Completed,
    Failed,
    Declined,
    TimedOut,
    Cancelled,
    InfrastructureError,
}

/// Direct evidence category derived from the completed Codex command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VerificationEvidenceKind {
    Command,
    Test,
}

/// Prepares a direct Command evidence product from one already-observed Codex
/// `ExecCommandEnd` outcome. Callers may not use model prose as evidence.
///
/// # Errors
///
/// Rejects non-verification roles and exit codes outside the transport range.
pub fn prepare_verification_command_evidence(
    job: &ExecutionJob,
    kind: VerificationEvidenceKind,
    status: VerificationEvidenceStatus,
    exit_code: i64,
    source_id: &str,
) -> Result<PreparedStageProduct, StageProductError> {
    ensure_verification_role(job)?;
    if i32::try_from(exit_code).is_err() || !bounded_text(source_id) {
        return Err(invalid_output());
    }
    prepare_json_product(
        match kind {
            VerificationEvidenceKind::Command => ExecutionEventCategory::Command,
            VerificationEvidenceKind::Test => ExecutionEventCategory::Test,
        },
        &VerificationCommandEvidence {
            source_id,
            status,
            exit_code,
        },
        match kind {
            VerificationEvidenceKind::Command => "verification command produced direct evidence",
            VerificationEvidenceKind::Test => "verification test produced direct evidence",
        },
    )
}

/// Converts the verification role's final assistant message into the strict
/// result Activity consumed by the production verdict authority.
///
/// # Errors
///
/// Rejects another role, malformed/noncanonical JSON, unknown fields, stale
/// protocol shape, malformed candidate identity, or an empty finding set.
pub fn prepare_verification_result_activity(
    job: &ExecutionJob,
    final_message: &[u8],
) -> Result<PreparedStageProduct, StageProductError> {
    ensure_verification_role(job)?;
    if final_message.is_empty() || final_message.len() > MAX_PLANNER_SOLUTION_BYTES {
        return Err(invalid_output());
    }
    let result: VerificationResultV1 =
        serde_json::from_slice(final_message).map_err(|_| invalid_output())?;
    if !result.is_structurally_valid()
        || !verification_matches_stage_input(&result, validate_stage_input(job)?)
    {
        return Err(invalid_output());
    }
    let canonical = serde_json::to_vec(&result).map_err(|_| invalid_output())?;
    if canonical != final_message {
        return Err(StageProductError::new(
            StageProductErrorCode::NonCanonicalOutput,
            "verification final response is not canonical JSON",
        ));
    }
    Ok(PreparedStageProduct {
        category: ExecutionEventCategory::Activity,
        media_type: VERIFICATION_JSON_MEDIA_TYPE,
        digest: Sha256Digest(format!("sha256:{:x}", Sha256::digest(&canonical))),
        bytes: canonical,
        summary: "verification role produced a canonical independent result",
    })
}

fn ensure_verification_role(job: &ExecutionJob) -> Result<(), StageProductError> {
    let Some(policy) = role_session_policy(job)? else {
        return Err(StageProductError::new(
            StageProductErrorCode::InvalidScope,
            "verification product requires a Delivery-stage execution scope",
        ));
    };
    if !matches!(
        policy.role_id.as_str(),
        "reviewer" | "verifier" | "adversarial-verifier"
    ) {
        return Err(StageProductError::new(
            StageProductErrorCode::InvalidRole,
            "verification product requires an independent verification role",
        ));
    }
    Ok(())
}

fn prepare_json_product<T: Serialize>(
    category: ExecutionEventCategory,
    value: &T,
    summary: &'static str,
) -> Result<PreparedStageProduct, StageProductError> {
    let bytes = serde_json::to_vec(value).map_err(|_| invalid_output())?;
    let digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(&bytes)));
    Ok(PreparedStageProduct {
        category,
        media_type: VERIFICATION_JSON_MEDIA_TYPE,
        bytes,
        digest,
        summary,
    })
}

fn valid_candidate_ref(value: &str) -> bool {
    value
        .strip_prefix("git-candidate:sha256:")
        .is_some_and(|digest| {
            digest.len() == 64
                && digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        })
}

fn planner_matches_stage_input(product: &PlannerSolutionV1, input: &DeliveryStageInput) -> bool {
    let available = input
        .acceptance_criteria
        .iter()
        .map(|criterion| criterion.criterion_id.as_str())
        .collect::<HashSet<_>>();
    let required = input
        .acceptance_criteria
        .iter()
        .filter(|criterion| criterion.required)
        .map(|criterion| criterion.criterion_id.as_str())
        .collect::<HashSet<_>>();
    let proposal_ids = product
        .task_proposals
        .iter()
        .map(|proposal| proposal.id.as_str())
        .collect::<HashSet<_>>();
    let assigned = product
        .task_proposals
        .iter()
        .flat_map(|proposal| proposal.acceptance_criterion_ids.iter().map(String::as_str))
        .collect::<HashSet<_>>();
    proposal_ids.len() == product.task_proposals.len()
        && product.task_proposals.iter().all(|proposal| {
            !proposal.acceptance_criterion_ids.is_empty()
                && proposal
                    .acceptance_criterion_ids
                    .iter()
                    .all(|criterion| available.contains(criterion.as_str()))
                && proposal
                    .blocked_by_task_ids
                    .iter()
                    .all(|dependency| proposal_ids.contains(dependency.as_str()))
                && !proposal
                    .blocked_by_task_ids
                    .iter()
                    .any(|dependency| dependency == &proposal.id)
        })
        && required.is_subset(&assigned)
}

fn verification_matches_stage_input(
    result: &VerificationResultV1,
    input: &DeliveryStageInput,
) -> bool {
    let Some(task) = input.task.as_ref() else {
        return false;
    };
    let expected = task
        .acceptance_criterion_ids
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    let actual = result
        .findings
        .iter()
        .filter_map(|finding| finding.criterion_id.as_deref())
        .collect::<HashSet<_>>();
    result.delivery_spec_id == input.delivery_spec_id
        && i64::try_from(result.delivery_spec_revision).ok() == Some(input.delivery_spec_revision)
        && input.candidate_ref.as_ref() == Some(&result.candidate_ref)
        && actual.len() == result.findings.len()
        && actual == expected
}

fn invalid_output() -> StageProductError {
    StageProductError::new(
        StageProductErrorCode::InvalidOutput,
        "Planner final response does not follow the canonical Solution Review protocol",
    )
}

#[derive(Clone, Copy)]
struct CanonicalRole {
    workspace_mode: &'static str,
    developer_instructions: &'static str,
}

fn canonical_role(role: &str) -> Option<CanonicalRole> {
    Some(match role {
        "requirements" => CanonicalRole {
            workspace_mode: "source-read-only",
            developer_instructions: "Turn the user request and verified repository facts into a proposed DeliverySpec with explicit scope, constraints, acceptance criteria, risks, and unresolved questions. Keep requirements separate from solution choices. Do not approve the proposal or start implementation.",
        },
        "solution" => CanonicalRole {
            workspace_mode: "source-read-only",
            developer_instructions: "Prepare a solution proposal for the exact approved DeliverySpec. Include structured system architecture and process-flow diagram data with stable node identities, components, connections, trust boundaries, external systems, and unresolved facts. Do not approve the proposal or modify the candidate.",
        },
        "planner" => CanonicalRole {
            workspace_mode: "source-read-only",
            developer_instructions: "Plan the approved delivery with Codex plan and multi-agent capabilities. Keep the work bounded by the approved DeliverySpec and solution, make verification explicit, and do not create a second task graph, modify candidate files, or declare delivery complete.",
        },
        "executor" => CanonicalRole {
            workspace_mode: "candidate-write",
            developer_instructions: "Implement only the approved delivery plan in the assigned candidate workspace. Use Codex tools, sandbox, approvals, plan, and subagents as needed. Preserve exact changed-file, command, test, diff, failure, recovery, and usage events. Do not approve or verify your own work.",
        },
        "reviewer" => CanonicalRole {
            workspace_mode: "candidate-read-only",
            developer_instructions: "Independently review the exact frozen candidate against the approved DeliverySpec and plan from a read-only workspace. Cite only observed Codex event evidence. The final response must follow the supplied winwincode.independent-verification-result.v1 JSON protocol. Do not modify the candidate or decide final delivery.",
        },
        "verifier" => CanonicalRole {
            workspace_mode: "candidate-read-only",
            developer_instructions: "Independently verify every assigned acceptance criterion against the exact frozen candidate from a read-only workspace. Run checks through Codex Core, cite only observed Codex event evidence, and return the supplied winwincode.independent-verification-result.v1 JSON protocol. Do not modify the candidate or decide final delivery.",
        },
        "adversarial-verifier" => CanonicalRole {
            workspace_mode: "candidate-read-only",
            developer_instructions: "Challenge the exact frozen candidate, approved assumptions, trust boundaries, failure handling, and negative cases from a read-only workspace. Cite reproducible Codex event evidence and return the supplied winwincode.independent-verification-result.v1 JSON protocol. Do not modify the candidate or decide final delivery.",
        },
        "remediator" => CanonicalRole {
            workspace_mode: "candidate-write",
            developer_instructions: "Apply only the bounded rework requested from reviewed findings in the assigned candidate workspace. Use Codex tools, sandbox, approvals, plan, and subagents as needed, preserve unrelated accepted work, and produce fresh runtime evidence. Do not broaden scope, approve, or verify your own work.",
        },
        _ => return None,
    })
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PlannerSolutionV1 {
    schema_version: u8,
    protocol: String,
    solution: PlannerSolution,
    architecture_diagram: PlannerDiagram,
    process_diagram: PlannerDiagram,
    risks: Vec<String>,
    unresolved_items: Vec<String>,
    task_proposals: Vec<PlannerTaskProposal>,
}

impl PlannerSolutionV1 {
    fn has_required_content(&self) -> bool {
        !self.solution.id.trim().is_empty()
            && !self.solution.summary.trim().is_empty()
            && !self.solution.approach.is_empty()
            && !self.solution.components.is_empty()
            && !self.architecture_diagram.nodes.is_empty()
            && !self.process_diagram.nodes.is_empty()
            && !self.task_proposals.is_empty()
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PlannerSolution {
    id: String,
    summary: String,
    approach: Vec<String>,
    components: Vec<PlannerSolutionComponent>,
    connections: Vec<PlannerConnection>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PlannerSolutionComponent {
    id: String,
    label: String,
    responsibility: String,
    kind: PlannerSolutionComponentKind,
    trust_boundary: Option<String>,
    unresolved: bool,
    repository_path_prefixes: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
enum PlannerSolutionComponentKind {
    #[serde(rename = "component")]
    Component,
    #[serde(rename = "data-store")]
    DataStore,
    #[serde(rename = "external")]
    External,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PlannerConnection {
    id: String,
    from: String,
    to: String,
    label: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PlannerDiagram {
    id: String,
    kind: PlannerDiagramKind,
    title: String,
    nodes: Vec<PlannerDiagramNode>,
    edges: Vec<PlannerConnection>,
}

#[derive(Debug, Deserialize, Serialize)]
enum PlannerDiagramKind {
    #[serde(rename = "system-architecture")]
    SystemArchitecture,
    #[serde(rename = "process-flow")]
    ProcessFlow,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PlannerDiagramNode {
    id: String,
    label: String,
    description: String,
    kind: PlannerDiagramNodeKind,
    trust_boundary: Option<String>,
    unresolved: bool,
}

#[derive(Debug, Deserialize, Serialize)]
enum PlannerDiagramNodeKind {
    #[serde(rename = "interaction")]
    Interaction,
    #[serde(rename = "delivery-control")]
    DeliveryControl,
    #[serde(rename = "execution")]
    Execution,
    #[serde(rename = "repository")]
    Repository,
    #[serde(rename = "component")]
    Component,
    #[serde(rename = "data-store")]
    DataStore,
    #[serde(rename = "decision")]
    Decision,
    #[serde(rename = "external")]
    External,
    #[serde(rename = "stage")]
    Stage,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PlannerTaskProposal {
    id: String,
    title: String,
    goal: String,
    acceptance_criterion_ids: Vec<String>,
    blocked_by_task_ids: Vec<String>,
}

#[derive(Serialize)]
struct VerificationPolicyAttestation<'candidate> {
    protocol: &'static str,
    workspace_mode: &'static str,
    permission_profile: &'static str,
    candidate_ref: &'candidate str,
}

#[derive(Serialize)]
struct VerificationCommandEvidence<'source> {
    source_id: &'source str,
    status: VerificationEvidenceStatus,
    exit_code: i64,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct VerificationResultV1 {
    protocol: String,
    delivery_spec_id: String,
    delivery_spec_revision: u64,
    candidate_ref: String,
    findings: Vec<VerificationFinding>,
}

impl VerificationResultV1 {
    fn is_structurally_valid(&self) -> bool {
        self.protocol == VERIFICATION_RESULT_PROTOCOL
            && bounded_text(&self.delivery_spec_id)
            && self.delivery_spec_revision > 0
            && valid_candidate_ref(&self.candidate_ref)
            && !self.findings.is_empty()
            && self.findings.iter().all(VerificationFinding::is_valid)
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct VerificationFinding {
    finding_id: String,
    criterion_id: Option<String>,
    verdict: VerificationVerdict,
    explanation: String,
    evidence_sources: Vec<VerificationEvidenceSource>,
}

impl VerificationFinding {
    fn is_valid(&self) -> bool {
        bounded_text(&self.finding_id)
            && self.criterion_id.as_deref().is_some_and(bounded_text)
            && bounded_text(&self.explanation)
            && matches!(
                self.verdict,
                VerificationVerdict::Pass | VerificationVerdict::Fail
            )
            && !self.evidence_sources.is_empty()
            && self
                .evidence_sources
                .iter()
                .all(VerificationEvidenceSource::is_valid)
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum VerificationVerdict {
    Pass,
    Fail,
    Inconclusive,
    InfraError,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct VerificationEvidenceSource {
    #[serde(rename = "type")]
    evidence_type: VerificationEvidenceType,
    event_id: String,
}

impl VerificationEvidenceSource {
    fn is_valid(&self) -> bool {
        matches!(
            self.evidence_type,
            VerificationEvidenceType::Test | VerificationEvidenceType::Command
        ) && bounded_text(&self.event_id)
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum VerificationEvidenceType {
    Test,
    Command,
    Diff,
    File,
    Commit,
    RuntimeEvent,
}

fn bounded_text(value: &str) -> bool {
    !value.trim().is_empty()
        && value.len() <= 4096
        && !value
            .bytes()
            .any(|byte| matches!(byte, 0..=8 | 11..=12 | 14..=31 | 127))
}

#[cfg(test)]
mod tests {
    use super::*;
    use winwincode_domain::{
        DeliveryId, DeliveryTaskId, ExecutionJobId, Instant, ProductSessionId, RepositoryId,
        StageRunId,
    };
    use winwincode_execution_port::generated::{
        DeliveryReworkAuthorizationScope, DeliveryStageAcceptanceCriterionInput,
        DeliveryStageExecutionScope, DeliveryStageExecutionScopeKind, DeliveryStageInput,
        DeliveryStageTaskInput, ExecutionLimits, ExecutionWorkspace, ExecutionWorkspaceWriteMode,
    };

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
        "\"goal\":\"Apply the accepted source change\",",
        "\"acceptanceCriterionIds\":[\"criterion-fixture\"],",
        "\"blockedByTaskIds\":[]",
        "}]",
        "}"
    );

    const CANDIDATE_REF: &str =
        "git-candidate:sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const VERIFICATION_RESULT_JSON: &str = concat!(
        "{\"protocol\":\"winwincode.independent-verification-result.v1\",",
        "\"delivery_spec_id\":\"spec-fixture\",",
        "\"delivery_spec_revision\":2,",
        "\"candidate_ref\":\"git-candidate:sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\",",
        "\"findings\":[{",
        "\"finding_id\":\"finding-reviewer-fixture\",",
        "\"criterion_id\":\"criterion-fixture\",",
        "\"verdict\":\"pass\",",
        "\"explanation\":\"The observed command completed successfully.\",",
        "\"evidence_sources\":[{",
        "\"type\":\"command\",",
        "\"event_id\":\"xevt_00000000000000000000000001\"",
        "}]",
        "}]",
        "}"
    );

    fn delivery_job(role: &str) -> ExecutionJob {
        let task_role = matches!(
            role,
            "executor" | "reviewer" | "verifier" | "adversarial-verifier" | "remediator"
        );
        let candidate_role = matches!(
            role,
            "reviewer" | "verifier" | "adversarial-verifier" | "remediator"
        );
        let task_id = DeliveryTaskId("dtk_00000000000000000000000001".to_owned());
        let rework_authorization =
            (role == "remediator").then(|| DeliveryReworkAuthorizationScope {
                authorization_digest: Sha256Digest(format!("sha256:{}", "b".repeat(64))),
                candidate_ref: CANDIDATE_REF.to_owned(),
                diff_sha256: "c".repeat(64),
                requires_full_reverification: true,
                source_candidate_commit_id: "d".repeat(40),
                source_candidate_tree_id: "e".repeat(40),
                targets: Vec::new(),
            });
        ExecutionJob {
            attempt: 1,
            execution_profile: role.to_owned(),
            goal: "Implement fixture".to_owned(),
            job_id: ExecutionJobId("job_00000000000000000000000001".to_owned()),
            limits: ExecutionLimits {
                deadline_at: Instant("2026-08-28T00:00:00Z".to_owned()),
                max_artifact_bytes: 1_048_576,
                max_runtime_seconds: 300,
            },
            payload_digest: Sha256Digest(
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    .to_owned(),
            ),
            scope: ExecutionScope::DeliveryStageExecutionScope(DeliveryStageExecutionScope {
                delivery_id: DeliveryId("dlv_00000000000000000000000001".to_owned()),
                delivery_task_id: task_role.then(|| task_id.clone()),
                kind: DeliveryStageExecutionScopeKind::DeliveryStage,
                product_session_id: ProductSessionId("ses_00000000000000000000000001".to_owned()),
                rework_authorization,
                stage_run_id: StageRunId("run_00000000000000000000000001".to_owned()),
            }),
            stage_input: Some(DeliveryStageInput {
                acceptance_criteria: vec![DeliveryStageAcceptanceCriterionInput {
                    criterion_id: "criterion-fixture".to_owned(),
                    description: "The exact fixture behavior is verified.".to_owned(),
                    required: true,
                    verification_method: Some("Run the exact fixture check.".to_owned()),
                }],
                candidate_ref: candidate_role.then(|| CANDIDATE_REF.to_owned()),
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
                    title: "Implement fixture".to_owned(),
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

    #[test]
    fn every_delivery_role_gets_the_canonical_workspace_policy() {
        for (role, mode) in [
            ("requirements", "source-read-only"),
            ("solution", "source-read-only"),
            ("planner", "source-read-only"),
            ("executor", "candidate-write"),
            ("reviewer", "candidate-read-only"),
            ("verifier", "candidate-read-only"),
            ("adversarial-verifier", "candidate-read-only"),
            ("remediator", "candidate-write"),
        ] {
            let policy = role_session_policy(&delivery_job(role))
                .expect("canonical role policy")
                .expect("Delivery role");
            assert_eq!(policy.schema_version, 1);
            assert_eq!(policy.role_id, role);
            assert_eq!(policy.workspace_mode, mode);
            assert!(!policy.developer_instructions.trim().is_empty());
        }
    }

    #[test]
    fn delivery_role_rejects_the_opposite_workspace_write_mode() {
        let mut executor = delivery_job("executor");
        executor.workspace.write_mode = ExecutionWorkspaceWriteMode::ReadOnly;
        assert_eq!(
            role_session_policy(&executor)
                .expect_err("writer role cannot use a read-only checkout")
                .code(),
            StageProductErrorCode::InvalidScope
        );

        let mut reviewer = delivery_job("reviewer");
        reviewer.workspace.write_mode = ExecutionWorkspaceWriteMode::Candidate;
        assert_eq!(
            role_session_policy(&reviewer)
                .expect_err("read-only role cannot use a writable checkout")
                .code(),
            StageProductErrorCode::InvalidScope
        );
    }

    #[test]
    fn planner_final_message_becomes_the_exact_activity_product() {
        let product =
            prepare_planner_solution_activity(&delivery_job("planner"), PLANNER_JSON.as_bytes())
                .expect("Planner Activity");
        assert_eq!(product.category(), &ExecutionEventCategory::Activity);
        assert_eq!(product.media_type(), PLANNER_SOLUTION_MEDIA_TYPE);
        assert_eq!(product.bytes(), PLANNER_JSON.as_bytes());
        assert_eq!(
            product.digest().0,
            format!("sha256:{:x}", Sha256::digest(PLANNER_JSON.as_bytes()))
        );
    }

    #[test]
    fn typed_stage_input_is_the_only_delivery_prompt_source() {
        let planner = delivery_job("planner");
        let prompt = stage_product_prompt(&planner).expect("typed Planner prompt");
        let encoded = serde_json::to_string(planner.stage_input.as_ref().expect("stage input"))
            .expect("canonical stage input JSON");
        assert!(prompt.contains(&encoded));
        assert!(prompt.contains(PLANNER_SOLUTION_PROTOCOL));
        assert!(prompt.contains("criterion-fixture"));

        let mut missing = planner.clone();
        missing.stage_input = None;
        assert_eq!(
            stage_product_prompt(&missing)
                .expect_err("Delivery prompt requires typed input")
                .code(),
            StageProductErrorCode::InvalidJob
        );

        let mut chat = planner;
        chat.execution_profile = "default".to_owned();
        chat.scope = ExecutionScope::ProductSessionExecutionScope(
            winwincode_execution_port::generated::ProductSessionExecutionScope {
                kind: winwincode_execution_port::generated::ProductSessionExecutionScopeKind::ProductSession,
                product_session_id: ProductSessionId(
                    "psn_00000000000000000000000001".to_owned(),
                ),
            },
        );
        chat.stage_input = None;
        assert_eq!(
            stage_product_prompt(&chat).expect("ProductSession goal"),
            chat.goal
        );
    }

    #[test]
    fn planner_product_rejects_another_role_and_noncanonical_bytes() {
        let role_error =
            prepare_planner_solution_activity(&delivery_job("executor"), PLANNER_JSON.as_bytes())
                .expect_err("executor cannot publish a Planner product");
        assert_eq!(role_error.code(), StageProductErrorCode::InvalidRole);

        let spaced = format!(" {PLANNER_JSON}");
        let canonical_error =
            prepare_planner_solution_activity(&delivery_job("planner"), spaced.as_bytes())
                .expect_err("noncanonical JSON is rejected");
        assert_eq!(
            canonical_error.code(),
            StageProductErrorCode::NonCanonicalOutput
        );

        let foreign_criterion = PLANNER_JSON.replace("criterion-fixture", "criterion-foreign");
        let input_error = prepare_planner_solution_activity(
            &delivery_job("planner"),
            foreign_criterion.as_bytes(),
        )
        .expect_err("Planner task cannot name a foreign criterion");
        assert_eq!(input_error.code(), StageProductErrorCode::InvalidOutput);
    }

    #[test]
    fn role_or_goal_change_changes_the_exact_stage_product_job_digest() {
        let planner = delivery_job("planner");
        let original = stage_product_job_digest(&planner).expect("job digest");
        let logical = stage_product_logical_job_digest(&planner).expect("logical job digest");
        assert_eq!(
            stage_product_job_digest(&planner).expect("replayed job digest"),
            original
        );

        let mut changed_role = planner.clone();
        changed_role.execution_profile = "executor".to_owned();
        assert_ne!(
            stage_product_job_digest(&changed_role).expect("changed role digest"),
            original
        );

        let mut changed_goal = planner;
        changed_goal.goal.push_str(" changed");
        assert_ne!(
            stage_product_job_digest(&changed_goal).expect("changed goal digest"),
            original
        );

        let mut changed_input = delivery_job("planner");
        changed_input
            .stage_input
            .as_mut()
            .expect("stage input")
            .delivery_spec_revision += 1;
        assert_ne!(
            stage_product_job_digest(&changed_input).expect("changed input digest"),
            original
        );

        let mut replacement_attempt = delivery_job("planner");
        replacement_attempt.attempt += 1;
        assert_ne!(
            stage_product_job_digest(&replacement_attempt).expect("replacement job digest"),
            original
        );
        assert_eq!(
            stage_product_logical_job_digest(&replacement_attempt)
                .expect("replacement logical digest"),
            logical
        );
    }

    #[test]
    fn delivery_role_cannot_be_attached_to_a_product_session_job() {
        let mut job = delivery_job("planner");
        job.scope = ExecutionScope::ProductSessionExecutionScope(
            winwincode_execution_port::generated::ProductSessionExecutionScope {
                kind: winwincode_execution_port::generated::ProductSessionExecutionScopeKind::ProductSession,
                product_session_id: ProductSessionId(
                    "ses_00000000000000000000000001".to_owned(),
                ),
            },
        );
        let error = role_session_policy(&job).expect_err("foreign scope");
        assert_eq!(error.code(), StageProductErrorCode::InvalidScope);
    }

    #[test]
    fn verification_products_form_policy_evidence_and_result_in_order() {
        let job = delivery_job("reviewer");
        let policy = prepare_verification_policy_attestation(&job, CANDIDATE_REF)
            .expect("policy attestation");
        assert_eq!(policy.category(), &ExecutionEventCategory::Lifecycle);
        assert_eq!(policy.media_type(), VERIFICATION_JSON_MEDIA_TYPE);
        assert_eq!(
            policy.bytes(),
            concat!(
                "{\"protocol\":\"winwincode.verification-session-policy.v1\",",
                "\"workspace_mode\":\"candidate-read-only\",",
                "\"permission_profile\":\"candidate-read-only-restricted\",",
                "\"candidate_ref\":\"git-candidate:sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"}"
            )
            .as_bytes()
        );

        let evidence = prepare_verification_command_evidence(
            &job,
            VerificationEvidenceKind::Command,
            VerificationEvidenceStatus::Completed,
            0,
            "call-fixture",
        )
        .expect("command evidence");
        assert_eq!(evidence.category(), &ExecutionEventCategory::Command);
        assert_eq!(
            evidence.bytes(),
            br#"{"source_id":"call-fixture","status":"completed","exit_code":0}"#
        );

        let result =
            prepare_verification_result_activity(&job, VERIFICATION_RESULT_JSON.as_bytes())
                .expect("verification result");
        assert_eq!(result.category(), &ExecutionEventCategory::Activity);
        assert_eq!(result.bytes(), VERIFICATION_RESULT_JSON.as_bytes());
    }

    #[test]
    fn verification_products_reject_writer_role_and_stale_candidate_shape() {
        let role_error =
            prepare_verification_policy_attestation(&delivery_job("executor"), CANDIDATE_REF)
                .expect_err("writer role cannot verify");
        assert_eq!(role_error.code(), StageProductErrorCode::InvalidRole);

        let candidate_error = prepare_verification_policy_attestation(
            &delivery_job("verifier"),
            "git-candidate:sha256:ABCDEF",
        )
        .expect_err("malformed candidate");
        assert_eq!(candidate_error.code(), StageProductErrorCode::InvalidOutput);

        let foreign_candidate = format!("git-candidate:sha256:{}", "b".repeat(64));
        let foreign_error =
            prepare_verification_policy_attestation(&delivery_job("verifier"), &foreign_candidate)
                .expect_err("well-shaped foreign candidate must fail");
        assert_eq!(foreign_error.code(), StageProductErrorCode::InvalidOutput);

        let spaced = format!(" {VERIFICATION_RESULT_JSON}");
        let result_error =
            prepare_verification_result_activity(&delivery_job("verifier"), spaced.as_bytes())
                .expect_err("noncanonical result");
        assert_eq!(
            result_error.code(),
            StageProductErrorCode::NonCanonicalOutput
        );

        let foreign_spec = VERIFICATION_RESULT_JSON.replace(
            "\"delivery_spec_revision\":2",
            "\"delivery_spec_revision\":3",
        );
        let spec_error = prepare_verification_result_activity(
            &delivery_job("verifier"),
            foreign_spec.as_bytes(),
        )
        .expect_err("foreign spec revision must fail");
        assert_eq!(spec_error.code(), StageProductErrorCode::InvalidOutput);
    }
}
