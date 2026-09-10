// SPDX-License-Identifier: Apache-2.0

//! Canonical semantic validation for one bounded `DebugProbe` round.
//!
//! The generated module owns wire shape. This module owns relationships that
//! JSON Schema cannot express: content-derived digests, exact authority and
//! intent bindings, aggregate budgets, receipt consistency, and ordered event
//! transitions. Host command admission and resource recomputation remain
//! Worker responsibilities and must happen after this seal and before spawn.

use std::{collections::HashSet, fmt};

use sha2::{Digest as _, Sha256};
use winwincode_domain::{ProbeExecutionId, ProbeId, Sha256Digest};

use crate::generated::{
    DebugProbeError, DebugProbeErrorCode, DebugProbeIdentity, DebugProbeKind, DebugProbePlan,
    DebugProbeRoundAuthority, ProbeCompletionRule, ProbeCompletionRuleKind, ProbeExecutionEvent,
    ProbeExecutionEventKind, ProbeExecutionIntent, ProbeExecutionReceipt, ProbeExecutionStatus,
    ProbeNetworkAccess, ProbeReceiptStatus, ProbeResourceClaim, ProbeRoundBudget,
    ProbeRoundCompletionReason, ProbeRoundEvent, ProbeRoundEventKind, ProbeRoundReceipt,
    ProbeRoundReceiptStatus, ProbeRoundStatus, ProbeSideEffectClass, ProbeSpec,
    ProbeWorkspaceAccess,
};

const COMMAND_ARG_BYTES_MAX: u64 = 262_144;
const MAX_SAFE_SEQUENCE: i64 = 9_007_199_254_740_991;
const PROBE_DIGEST_DOMAIN: &[u8] = b"winwincode.debug-probe.definition.v1\0";
const BUDGET_DIGEST_DOMAIN: &[u8] = b"winwincode.debug-probe.budget.v1\0";
const PLAN_DIGEST_DOMAIN: &[u8] = b"winwincode.debug-probe.plan.v1\0";
const EXECUTION_ID_DOMAIN: &[u8] = b"winwincode.debug-probe.execution-id.v1\0";

/// Secret-safe semantic validation failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DebugProbeContractError {
    code: DebugProbeErrorCode,
    message: &'static str,
}

impl DebugProbeContractError {
    /// Returns the canonical wire error category.
    #[must_use]
    pub const fn code(&self) -> &DebugProbeErrorCode {
        &self.code
    }

    /// Returns a bounded message that never echoes command or resource input.
    #[must_use]
    pub const fn message(&self) -> &'static str {
        self.message
    }
}

impl fmt::Display for DebugProbeContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for DebugProbeContractError {}

/// One probe whose wire shape, content digest, budget, and execution identity
/// were sealed against its parent plan.
#[derive(Clone, Debug, PartialEq)]
pub struct ValidatedProbe {
    ordinal: usize,
    spec: ProbeSpec,
    execution_id: ProbeExecutionId,
    command_arg_bytes: i64,
}

impl ValidatedProbe {
    /// Stable zero-based position in the canonical plan.
    #[must_use]
    pub const fn ordinal(&self) -> usize {
        self.ordinal
    }

    /// Exact generated specification retained by the plan.
    #[must_use]
    pub const fn spec(&self) -> &ProbeSpec {
        &self.spec
    }

    /// Host-derived identity for this exact plan authority and definition.
    #[must_use]
    pub const fn execution_id(&self) -> &ProbeExecutionId {
        &self.execution_id
    }

    /// Canonical encoded argv byte count verified during sealing.
    #[must_use]
    pub const fn command_arg_bytes(&self) -> i64 {
        self.command_arg_bytes
    }
}

/// An owned plan that passed every contract-level semantic check.
#[derive(Clone, Debug, PartialEq)]
pub struct ValidatedDebugProbePlan {
    plan: DebugProbePlan,
    probes: Vec<ValidatedProbe>,
}

impl ValidatedDebugProbePlan {
    /// Exact generated plan retained by the seal.
    #[must_use]
    pub const fn plan(&self) -> &DebugProbePlan {
        &self.plan
    }

    /// Canonically ordered sealed probes.
    #[must_use]
    pub fn probes(&self) -> &[ValidatedProbe] {
        &self.probes
    }

    /// Finds one sealed probe by its plan-local identity.
    #[must_use]
    pub fn probe_by_id(&self, probe_id: &ProbeId) -> Option<&ValidatedProbe> {
        self.probes
            .iter()
            .find(|probe| probe.spec.probe_id == *probe_id)
    }

    /// Builds the canonical identity for one probe in this sealed plan.
    #[must_use]
    pub fn probe_identity(&self, probe_id: &ProbeId) -> Option<DebugProbeIdentity> {
        self.probe_by_id(probe_id)
            .map(|probe| probe_identity(&self.plan.authority, probe))
    }

    /// Consumes the seal and returns the original generated plan.
    #[must_use]
    pub fn into_plan(self) -> DebugProbePlan {
        self.plan
    }
}

/// An owned intent bound to one sealed probe and plan.
#[derive(Clone, Debug, PartialEq)]
pub struct ValidatedProbeExecutionIntent {
    intent: ProbeExecutionIntent,
    probe: ValidatedProbe,
}

impl ValidatedProbeExecutionIntent {
    /// Exact generated intent retained by the seal.
    #[must_use]
    pub const fn intent(&self) -> &ProbeExecutionIntent {
        &self.intent
    }

    /// Sealed probe addressed by this intent.
    #[must_use]
    pub const fn probe(&self) -> &ValidatedProbe {
        &self.probe
    }

    /// Consumes the seal and returns the original generated intent.
    #[must_use]
    pub fn into_intent(self) -> ProbeExecutionIntent {
        self.intent
    }
}

/// Returns the byte length of the canonical argv encoding.
///
/// The encoding is an unsigned 64-bit big-endian argument count followed by
/// each UTF-8 argument framed with its unsigned 64-bit big-endian byte length.
/// This is independent of shell quoting and platform argument rendering.
///
/// # Errors
///
/// Rejects an empty, oversized, NUL-containing, or aggregate-oversized argv.
pub fn derive_probe_command_arg_bytes(argv: &[String]) -> Result<i64, DebugProbeContractError> {
    if argv.is_empty() || argv.len() > 64 {
        return Err(invalid_probe("probe argv count is invalid"));
    }
    let mut bytes = 8_u64;
    for argument in argv {
        if argument.is_empty() || argument.chars().count() > 4_096 || argument.contains('\0') {
            return Err(invalid_probe("probe argv value is invalid"));
        }
        let length = u64::try_from(argument.len())
            .map_err(|_| invalid_probe("probe argv byte count overflowed"))?;
        bytes = bytes
            .checked_add(8)
            .and_then(|value| value.checked_add(length))
            .ok_or_else(|| invalid_probe("probe argv byte count overflowed"))?;
    }
    if bytes > COMMAND_ARG_BYTES_MAX {
        return Err(invalid_probe("probe argv exceeds its byte limit"));
    }
    i64::try_from(bytes).map_err(|_| invalid_probe("probe argv byte count overflowed"))
}

/// Derives the canonical digest of a probe definition, excluding the digest
/// field itself.
///
/// # Errors
///
/// Rejects a structurally or semantically invalid specification.
pub fn derive_probe_definition_digest(
    spec: &ProbeSpec,
) -> Result<Sha256Digest, DebugProbeContractError> {
    validate_probe_spec_shape(spec)?;
    let command_arg_bytes = derive_probe_command_arg_bytes(&spec.command.argv)?;
    let mut digest = FramedDigest::new(PROBE_DIGEST_DOMAIN);
    digest.text(&spec.probe_id.0)?;
    digest.text(probe_kind_tag(&spec.kind))?;
    digest.list_len(spec.command.argv.len())?;
    for argument in &spec.command.argv {
        digest.text(argument)?;
    }
    digest.text(&spec.command.working_directory)?;
    digest.i64(command_arg_bytes);
    hash_resource_claim(&mut digest, &spec.resources)?;
    digest.i64(spec.timeout_millis);
    digest.i64(spec.output_limit_bytes);
    digest.boolean(spec.required);
    let mut hypothesis_ids = spec
        .target_hypothesis_ids
        .iter()
        .map(|identity| identity.0.as_str())
        .collect::<Vec<_>>();
    hypothesis_ids.sort_unstable();
    digest.list_len(hypothesis_ids.len())?;
    for identity in hypothesis_ids {
        digest.text(identity)?;
    }
    Ok(digest.finish())
}

/// Derives the canonical digest of a round budget, excluding the digest field
/// itself.
///
/// # Errors
///
/// Rejects values outside the generated hard bounds.
pub fn derive_probe_budget_digest(
    budget: &ProbeRoundBudget,
) -> Result<Sha256Digest, DebugProbeContractError> {
    validate_budget_shape(budget)?;
    let mut digest = FramedDigest::new(BUDGET_DIGEST_DOMAIN);
    digest.i64(budget.probe_limit);
    digest.i64(budget.parallel_probe_limit);
    digest.i64(budget.wall_time_limit_millis);
    digest.i64(budget.total_output_limit_bytes);
    digest.i64(budget.total_cpu_limit_millis);
    digest.i64(budget.peak_memory_limit_bytes);
    digest.i64(budget.total_command_arg_limit_bytes);
    Ok(digest.finish())
}

/// Derives the canonical digest of one plan, excluding all supplied digest
/// fields and using freshly derived probe and budget digests.
///
/// Probe order is significant. Set-valued fields inside a probe are sorted by
/// their canonical text or numeric representation before hashing.
///
/// # Errors
///
/// Rejects invalid authority, plan, probe, budget, or completion-rule input.
pub fn derive_debug_probe_plan_digest(
    plan: &DebugProbePlan,
) -> Result<Sha256Digest, DebugProbeContractError> {
    validate_plan_shape(plan)?;
    let mut digest = FramedDigest::new(PLAN_DIGEST_DOMAIN);
    digest.i64(plan.schema_version);
    hash_round_authority(&mut digest, &plan.authority)?;
    digest.list_len(plan.probes.len())?;
    for probe in &plan.probes {
        digest.text(&derive_probe_definition_digest(probe)?.0)?;
    }
    digest.text(&derive_probe_budget_digest(&plan.budget)?.0)?;
    hash_completion_rule(&mut digest, &plan.completion_rule)?;
    digest.text(&plan.created_at.0)?;
    Ok(digest.finish())
}

/// Derives the identity for one exact scheduled probe execution.
///
/// # Errors
///
/// Rejects malformed authority, plan digest, or probe input.
pub fn derive_probe_execution_id(
    authority: &DebugProbeRoundAuthority,
    plan_digest: &Sha256Digest,
    spec: &ProbeSpec,
) -> Result<ProbeExecutionId, DebugProbeContractError> {
    validate_authority_shape(authority)?;
    require_digest(plan_digest, "probe plan digest is invalid")?;
    let definition_digest = derive_probe_definition_digest(spec)?;
    let mut digest = FramedDigest::new(EXECUTION_ID_DOMAIN);
    hash_round_authority(&mut digest, authority)?;
    digest.text(&plan_digest.0)?;
    digest.text(&spec.probe_id.0)?;
    digest.text(&definition_digest.0)?;
    Ok(ProbeExecutionId(digest.finish().0))
}

/// Confirms that presented round authority exactly matches the current host
/// authority.
///
/// # Errors
///
/// Rejects malformed current authority or any stale/foreign presented field.
pub fn validate_debug_probe_round_authority(
    actual: &DebugProbeRoundAuthority,
    expected: &DebugProbeRoundAuthority,
) -> Result<(), DebugProbeContractError> {
    validate_authority_shape(expected)
        .map_err(|_| invalid_plan("current DebugProbe authority is invalid"))?;
    validate_authority_shape(actual)?;
    if actual != expected {
        return Err(stale_authority("DebugProbe round authority is stale"));
    }
    Ok(())
}

/// Seals one generated plan against current host authority.
///
/// # Errors
///
/// Rejects stale authority, changed derivations, duplicate probe IDs, aggregate
/// budget overflow, and unsupported `first_conclusive` completion.
pub fn seal_debug_probe_plan(
    plan: DebugProbePlan,
    expected_authority: &DebugProbeRoundAuthority,
) -> Result<ValidatedDebugProbePlan, DebugProbeContractError> {
    validate_debug_probe_round_authority(&plan.authority, expected_authority)?;
    validate_plan_shape(&plan)?;
    if matches!(
        plan.completion_rule.kind,
        ProbeCompletionRuleKind::FirstConclusive
    ) {
        return Err(invalid_plan(
            "first_conclusive has no host-owned conclusion fact in this contract",
        ));
    }

    let probe_count =
        i64::try_from(plan.probes.len()).map_err(|_| invalid_plan("probe count is invalid"))?;
    if probe_count > plan.budget.probe_limit
        || plan.budget.parallel_probe_limit > plan.budget.probe_limit
    {
        return Err(budget_exceeded("probe count exceeds the round budget"));
    }
    if matches!(
        plan.completion_rule.kind,
        ProbeCompletionRuleKind::MinimumSuccesses
    ) && (plan.completion_rule.minimum_completed_probes > probe_count
        || plan.completion_rule.minimum_successful_probes
            > plan.completion_rule.minimum_completed_probes)
    {
        return Err(invalid_plan(
            "probe completion threshold exceeds the plan size",
        ));
    }

    let expected_budget_digest = derive_probe_budget_digest(&plan.budget)?;
    if plan.budget.budget_digest != expected_budget_digest {
        return Err(invalid_plan(
            "probe budget digest does not match its fields",
        ));
    }

    let mut probe_ids = HashSet::with_capacity(plan.probes.len());
    let mut total_output = 0_i64;
    let mut total_cpu = 0_i64;
    let mut total_arg_bytes = 0_i64;
    for spec in &plan.probes {
        if !probe_ids.insert(spec.probe_id.0.as_str()) {
            return Err(invalid_plan("probe IDs must be unique within a plan"));
        }
        let command_arg_bytes = derive_probe_command_arg_bytes(&spec.command.argv)?;
        if command_arg_bytes != spec.command.command_arg_bytes {
            return Err(invalid_probe(
                "probe commandArgBytes does not match canonical argv",
            ));
        }
        if spec.probe_definition_digest != derive_probe_definition_digest(spec)? {
            return Err(invalid_probe(
                "probe definition digest does not match its fields",
            ));
        }
        if spec.timeout_millis > plan.budget.wall_time_limit_millis
            || spec.resources.memory_limit_bytes > plan.budget.peak_memory_limit_bytes
        {
            return Err(budget_exceeded("probe exceeds a per-round hard limit"));
        }
        total_output = checked_sum(total_output, spec.output_limit_bytes)?;
        total_cpu = checked_sum(total_cpu, spec.resources.cpu_limit_millis)?;
        total_arg_bytes = checked_sum(total_arg_bytes, command_arg_bytes)?;
    }
    if total_output > plan.budget.total_output_limit_bytes
        || total_cpu > plan.budget.total_cpu_limit_millis
        || total_arg_bytes > plan.budget.total_command_arg_limit_bytes
    {
        return Err(budget_exceeded(
            "aggregate probe declarations exceed the round budget",
        ));
    }

    if plan.plan_digest != derive_debug_probe_plan_digest(&plan)? {
        return Err(invalid_plan("probe plan digest does not match its fields"));
    }
    let probes = plan
        .probes
        .iter()
        .enumerate()
        .map(|(ordinal, spec)| {
            Ok(ValidatedProbe {
                ordinal,
                execution_id: derive_probe_execution_id(&plan.authority, &plan.plan_digest, spec)?,
                command_arg_bytes: derive_probe_command_arg_bytes(&spec.command.argv)?,
                spec: spec.clone(),
            })
        })
        .collect::<Result<Vec<_>, DebugProbeContractError>>()?;
    Ok(ValidatedDebugProbePlan { plan, probes })
}

/// Seals one durable intent against a validated plan.
///
/// # Errors
///
/// Rejects changed schema, plan, probe definition, authority, execution ID, or
/// creation order.
pub fn seal_probe_execution_intent(
    intent: ProbeExecutionIntent,
    plan: &ValidatedDebugProbePlan,
) -> Result<ValidatedProbeExecutionIntent, DebugProbeContractError> {
    if intent.schema_version != 1 || intent.plan_digest != plan.plan.plan_digest {
        return Err(invalid_probe("probe intent plan binding is invalid"));
    }
    let probe = plan
        .probe_by_id(&intent.spec.probe_id)
        .ok_or_else(|| invalid_probe("probe intent is not present in its plan"))?;
    if intent.spec != probe.spec {
        return Err(invalid_probe(
            "probe intent definition changed after sealing",
        ));
    }
    let expected_identity = probe_identity(&plan.plan.authority, probe);
    if intent.identity != expected_identity {
        return Err(stale_authority("probe intent authority is stale"));
    }
    if !canonical_instant(&intent.created_at.0) || intent.created_at.0 < plan.plan.created_at.0 {
        return Err(invalid_probe("probe intent creation time is invalid"));
    }
    Ok(ValidatedProbeExecutionIntent {
        intent,
        probe: probe.clone(),
    })
}

/// Validates one terminal execution receipt against its exact durable intent.
///
/// # Errors
///
/// Rejects identity, plan, time, output, artifact, or status/error drift.
pub fn validate_probe_execution_receipt(
    receipt: &ProbeExecutionReceipt,
    intent: &ValidatedProbeExecutionIntent,
) -> Result<(), DebugProbeContractError> {
    if receipt.schema_version != 1
        || receipt.identity != intent.intent.identity
        || receipt.plan_digest != intent.intent.plan_digest
    {
        return Err(stale_authority("probe receipt authority is stale"));
    }
    if !canonical_instant(&receipt.started_at.0)
        || !canonical_instant(&receipt.finished_at.0)
        || receipt.started_at.0 < intent.intent.created_at.0
        || receipt.finished_at.0 < receipt.started_at.0
        || !(0..=3_600_000).contains(&receipt.duration_millis)
    {
        return Err(invalid_probe("probe receipt timing is invalid"));
    }
    if receipt.output_bytes < 0
        || receipt.output_bytes > intent.probe.spec.output_limit_bytes
        || (receipt.output_truncated
            && receipt.output_bytes != intent.probe.spec.output_limit_bytes)
        || receipt.artifact_refs.len() > 8
        || !unique_artifacts(&receipt.artifact_refs)
    {
        return Err(invalid_probe("probe receipt output binding is invalid"));
    }
    validate_error_shape(receipt.error.as_ref())?;
    if receipt
        .exit_code
        .is_some_and(|code| !(-2_147_483_648..=2_147_483_647).contains(&code))
        || receipt.signal.as_ref().is_some_and(|signal| {
            signal.is_empty()
                || signal.len() > 64
                || !signal
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'+' | b'-'))
        })
        || (receipt.exit_code.is_some() && receipt.signal.is_some())
    {
        return Err(invalid_probe("probe receipt exit status is invalid"));
    }
    let status_valid = match receipt.status {
        ProbeReceiptStatus::Succeeded => {
            receipt.exit_code == Some(0)
                && receipt.signal.is_none()
                && !receipt.timed_out
                && receipt.error.is_none()
        }
        ProbeReceiptStatus::Failed => {
            !receipt.timed_out
                && receipt.error.as_ref().is_some_and(|error| {
                    !matches!(
                        error.code,
                        DebugProbeErrorCode::TimedOut
                            | DebugProbeErrorCode::Cancelled
                            | DebugProbeErrorCode::StaleAuthority
                    )
                })
        }
        ProbeReceiptStatus::TimedOut => {
            receipt.timed_out
                && receipt.exit_code.is_none()
                && error_is(receipt.error.as_ref(), &DebugProbeErrorCode::TimedOut)
        }
        ProbeReceiptStatus::Cancelled => {
            !receipt.timed_out
                && receipt.exit_code.is_none()
                && error_is(receipt.error.as_ref(), &DebugProbeErrorCode::Cancelled)
        }
        ProbeReceiptStatus::Skipped => {
            !receipt.timed_out
                && receipt.exit_code.is_none()
                && receipt.signal.is_none()
                && receipt.duration_millis == 0
                && receipt.output_bytes == 0
                && receipt.artifact_refs.is_empty()
                && receipt.error.is_none()
        }
        ProbeReceiptStatus::CacheHit => {
            !receipt.timed_out
                && receipt.exit_code.is_none()
                && receipt.signal.is_none()
                && receipt.duration_millis == 0
                && receipt.error.is_none()
        }
        ProbeReceiptStatus::Stale => {
            !receipt.timed_out
                && receipt.exit_code.is_none()
                && error_is(receipt.error.as_ref(), &DebugProbeErrorCode::StaleAuthority)
        }
    };
    if !status_valid {
        return Err(invalid_probe(
            "probe receipt status fields are inconsistent",
        ));
    }
    Ok(())
}

fn validate_reducer_supplement(
    supplement: Option<&crate::generated::ProbeRoundReducerSupplement>,
) -> Result<(), DebugProbeContractError> {
    let Some(supplement) = supplement else {
        return Ok(());
    };
    if supplement.schema_version != 1
        || !supplement.operation_id.starts_with("probe-reducer:sha256:")
        || supplement.operation_id.len() != 85
        || !(0..=1).contains(&supplement.provider_calls)
        || (supplement.provider_calls == 0 && supplement.usage.is_some())
        || (supplement.provider_calls == 0
            && !matches!(
                supplement.reason_code,
                crate::generated::ProbeReducerReasonCode::L0Sufficient
                    | crate::generated::ProbeReducerReasonCode::L1Sufficient
            ))
        || (matches!(
            supplement.reason_code,
            crate::generated::ProbeReducerReasonCode::ProviderCompleted
        ) && supplement.status != crate::generated::ProbeReducerStatus::Completed)
        || (matches!(
            supplement.reason_code,
            crate::generated::ProbeReducerReasonCode::ProviderRateLimited
                | crate::generated::ProbeReducerReasonCode::ProviderTimeout
                | crate::generated::ProbeReducerReasonCode::ProviderInfrastructure
                | crate::generated::ProbeReducerReasonCode::InvalidJson
                | crate::generated::ProbeReducerReasonCode::UnknownField
                | crate::generated::ProbeReducerReasonCode::PromptInjection
                | crate::generated::ProbeReducerReasonCode::OutputBudgetExceeded
        ) && supplement.status != crate::generated::ProbeReducerStatus::Inconclusive)
        || (supplement.provider_calls == 1
            && matches!(
                supplement.reason_code,
                crate::generated::ProbeReducerReasonCode::L0Sufficient
                    | crate::generated::ProbeReducerReasonCode::L1Sufficient
            ))
    {
        return Err(invalid_plan("probe reducer supplement is inconsistent"));
    }
    if supplement
        .supporting_hypothesis_ids
        .iter()
        .any(|id| supplement.contradicting_hypothesis_ids.contains(id))
    {
        return Err(invalid_plan("probe reducer hypothesis polarity overlaps"));
    }
    Ok(())
}

/// Validates the complete strictly ordered event history for one probe.
///
/// # Errors
///
/// Rejects gaps, changed identity, illegal transitions, post-terminal events,
/// invalid times, or artifacts before the terminal event.
pub fn validate_probe_execution_events(
    events: &[ProbeExecutionEvent],
    intent: &ValidatedProbeExecutionIntent,
) -> Result<(), DebugProbeContractError> {
    if events.is_empty() || events.len() > 3 {
        return Err(invalid_probe("probe event count is invalid"));
    }
    let mut previous_time = intent.intent.created_at.0.as_str();
    let mut started = false;
    let mut finished = false;
    for (index, event) in events.iter().enumerate() {
        let expected_sequence = i64::try_from(index + 1)
            .map_err(|_| invalid_probe("probe event sequence overflowed"))?;
        if event.identity != intent.intent.identity
            || event.sequence.0 != expected_sequence
            || event.sequence.0 > MAX_SAFE_SEQUENCE
            || !canonical_instant(&event.occurred_at.0)
            || event.occurred_at.0.as_str() < previous_time
            || !bounded_summary(&event.summary)
            || event.artifact_refs.len() > 8
            || !unique_artifacts(&event.artifact_refs)
            || finished
        {
            return Err(invalid_probe("probe event binding or order is invalid"));
        }
        previous_time = &event.occurred_at.0;
        match (&event.kind, &event.status, index) {
            (ProbeExecutionEventKind::Scheduled, ProbeExecutionStatus::Scheduled, 0)
                if event.artifact_refs.is_empty() => {}
            (ProbeExecutionEventKind::Started, ProbeExecutionStatus::Running, 1)
                if event.artifact_refs.is_empty() =>
            {
                started = true;
            }
            (ProbeExecutionEventKind::Finished, status, 1 | 2)
                if terminal_probe_status(status)
                    && (started
                        || matches!(
                            status,
                            ProbeExecutionStatus::Skipped
                                | ProbeExecutionStatus::CacheHit
                                | ProbeExecutionStatus::Cancelled
                                | ProbeExecutionStatus::Stale
                        )) =>
            {
                finished = true;
            }
            _ => return Err(invalid_probe("probe event transition is invalid")),
        }
    }
    Ok(())
}

/// Validates a terminal probe event history against its terminal receipt.
///
/// # Errors
///
/// Returns the event or receipt error, or rejects a missing/changed terminal
/// status, finish time, or Artifact projection.
pub fn validate_probe_execution_history(
    events: &[ProbeExecutionEvent],
    receipt: &ProbeExecutionReceipt,
    intent: &ValidatedProbeExecutionIntent,
) -> Result<(), DebugProbeContractError> {
    validate_probe_execution_events(events, intent)?;
    validate_probe_execution_receipt(receipt, intent)?;
    let terminal = events
        .last()
        .filter(|event| matches!(event.kind, ProbeExecutionEventKind::Finished))
        .ok_or_else(|| invalid_probe("probe terminal event is missing"))?;
    if !event_status_matches_receipt(&terminal.status, &receipt.status)
        || terminal.occurred_at != receipt.finished_at
        || terminal.artifact_refs != receipt.artifact_refs
    {
        return Err(invalid_probe(
            "probe terminal event and receipt do not agree",
        ));
    }
    Ok(())
}

/// Validates the complete strictly ordered event history for one probe round.
///
/// # Errors
///
/// Rejects gaps, changed authority, illegal transitions, post-terminal events,
/// or invalid time/summary values.
pub fn validate_probe_round_events(
    events: &[ProbeRoundEvent],
    plan: &ValidatedDebugProbePlan,
) -> Result<(), DebugProbeContractError> {
    if events.is_empty() || events.len() > 3 {
        return Err(invalid_plan("probe round event count is invalid"));
    }
    let mut previous_time = plan.plan.created_at.0.as_str();
    for (index, event) in events.iter().enumerate() {
        let expected_sequence = i64::try_from(index + 1)
            .map_err(|_| invalid_plan("probe round event sequence overflowed"))?;
        if event.authority != plan.plan.authority
            || event.sequence.0 != expected_sequence
            || event.sequence.0 > MAX_SAFE_SEQUENCE
            || !canonical_instant(&event.occurred_at.0)
            || event.occurred_at.0.as_str() < previous_time
            || !bounded_summary(&event.summary)
        {
            return Err(invalid_plan(
                "probe round event binding or order is invalid",
            ));
        }
        previous_time = &event.occurred_at.0;
        let valid = matches!(
            (&event.kind, &event.status, index),
            (ProbeRoundEventKind::Planned, ProbeRoundStatus::Planned, 0)
                | (ProbeRoundEventKind::Started, ProbeRoundStatus::Running, 1)
                | (
                    ProbeRoundEventKind::Finished,
                    ProbeRoundStatus::Completed
                        | ProbeRoundStatus::Failed
                        | ProbeRoundStatus::Cancelled
                        | ProbeRoundStatus::Stale,
                    2
                )
        );
        if !valid {
            return Err(invalid_plan("probe round event transition is invalid"));
        }
    }
    Ok(())
}

/// Validates a terminal round event history against its terminal receipt.
///
/// # Errors
///
/// Returns the event or receipt error, or rejects a missing/changed terminal
/// status or finish time.
pub fn validate_probe_round_history(
    events: &[ProbeRoundEvent],
    receipt: &ProbeRoundReceipt,
    plan: &ValidatedDebugProbePlan,
) -> Result<(), DebugProbeContractError> {
    validate_probe_round_events(events, plan)?;
    validate_probe_round_receipt(receipt, plan)?;
    let terminal = events
        .last()
        .filter(|event| matches!(event.kind, ProbeRoundEventKind::Finished))
        .ok_or_else(|| invalid_plan("probe round terminal event is missing"))?;
    if !round_event_status_matches_receipt(&terminal.status, &receipt.status)
        || terminal.occurred_at != receipt.finished_at
    {
        return Err(invalid_plan(
            "probe round terminal event and receipt do not agree",
        ));
    }
    Ok(())
}

/// Validates one terminal round receipt and every nested probe receipt.
///
/// # Errors
///
/// Rejects authority, plan, coverage, order, status, completion, time, or
/// audited-budget drift.
pub fn validate_probe_round_receipt(
    receipt: &ProbeRoundReceipt,
    plan: &ValidatedDebugProbePlan,
) -> Result<(), DebugProbeContractError> {
    if receipt.schema_version != 1
        || receipt.authority != plan.plan.authority
        || receipt.plan_digest != plan.plan.plan_digest
    {
        return Err(stale_authority("probe round receipt authority is stale"));
    }
    if receipt.probe_receipts.len() != plan.probes.len()
        || !canonical_instant(&receipt.started_at.0)
        || !canonical_instant(&receipt.finished_at.0)
        || receipt.started_at.0 < plan.plan.created_at.0
        || receipt.finished_at.0 < receipt.started_at.0
    {
        return Err(invalid_plan(
            "probe round receipt coverage or timing is invalid",
        ));
    }

    let mut total_output = 0_i64;
    let mut completed = 0_i64;
    let mut successes = 0_i64;
    let mut required_failure = false;
    for (probe, probe_receipt) in plan.probes.iter().zip(&receipt.probe_receipts) {
        let synthetic_intent = ValidatedProbeExecutionIntent {
            intent: ProbeExecutionIntent {
                created_at: plan.plan.created_at.clone(),
                identity: probe_identity(&plan.plan.authority, probe),
                plan_digest: plan.plan.plan_digest.clone(),
                schema_version: 1,
                spec: probe.spec.clone(),
            },
            probe: probe.clone(),
        };
        validate_probe_execution_receipt(probe_receipt, &synthetic_intent)?;
        if probe_receipt.started_at.0 < receipt.started_at.0
            || probe_receipt.finished_at.0 > receipt.finished_at.0
        {
            return Err(invalid_plan(
                "probe receipt timing falls outside its round receipt",
            ));
        }
        total_output = checked_sum(total_output, probe_receipt.output_bytes)?;
        let success = matches!(
            probe_receipt.status,
            ProbeReceiptStatus::Succeeded | ProbeReceiptStatus::CacheHit
        );
        completed += i64::from(matches!(
            probe_receipt.status,
            ProbeReceiptStatus::Succeeded
                | ProbeReceiptStatus::Failed
                | ProbeReceiptStatus::TimedOut
                | ProbeReceiptStatus::CacheHit
        ));
        successes += i64::from(success);
        required_failure |= probe.spec.required && !success;
    }

    let usage = &receipt.usage;
    let budget = &plan.plan.budget;
    let probe_count = i64::try_from(plan.probes.len())
        .map_err(|_| invalid_plan("probe round receipt count overflowed"))?;
    let declared_command_arg_bytes = plan.probes.iter().try_fold(0_i64, |total, probe| {
        checked_sum(total, probe.command_arg_bytes)
    })?;
    let declared_cpu_millis = plan.probes.iter().try_fold(0_i64, |total, probe| {
        checked_sum(total, probe.spec.resources.cpu_limit_millis)
    })?;
    if usage.budget_digest != budget.budget_digest
        || usage.probe_count != probe_count
        || usage.peak_parallel_probes < 0
        || usage.peak_parallel_probes > budget.parallel_probe_limit
        || usage.elapsed_millis < 0
        || usage.elapsed_millis > budget.wall_time_limit_millis
        || usage.total_output_bytes != total_output
        || usage.total_output_bytes > budget.total_output_limit_bytes
        || usage.total_cpu_millis < 0
        || usage.total_cpu_millis > budget.total_cpu_limit_millis
        || usage.total_cpu_millis > declared_cpu_millis
        || usage.peak_memory_bytes < 0
        || usage.peak_memory_bytes > budget.peak_memory_limit_bytes
        || usage.total_command_arg_bytes < 0
        || usage.total_command_arg_bytes > budget.total_command_arg_limit_bytes
        || usage.total_command_arg_bytes > declared_command_arg_bytes
    {
        return Err(budget_exceeded(
            "probe round usage exceeds or contradicts its budget",
        ));
    }
    validate_error_shape(receipt.error.as_ref())?;
    validate_reducer_supplement(receipt.reducer.as_ref())?;

    if !round_receipt_status_valid(
        receipt,
        &plan.plan.completion_rule,
        completed,
        successes,
        required_failure,
    ) {
        return Err(invalid_plan("probe round receipt status is inconsistent"));
    }
    Ok(())
}

fn round_receipt_status_valid(
    receipt: &ProbeRoundReceipt,
    rule: &ProbeCompletionRule,
    completed: i64,
    successes: i64,
    required_failure: bool,
) -> bool {
    let completion_satisfied = match rule.kind {
        ProbeCompletionRuleKind::AllTerminal => true,
        ProbeCompletionRuleKind::MinimumSuccesses => {
            completed >= rule.minimum_completed_probes
                && successes >= rule.minimum_successful_probes
        }
        ProbeCompletionRuleKind::FirstConclusive => false,
    };
    match receipt.status {
        ProbeRoundReceiptStatus::Completed => {
            receipt.error.is_none()
                && completion_satisfied
                && !required_failure
                && matches!(
                    receipt.completion_reason,
                    ProbeRoundCompletionReason::CompletionRuleSatisfied
                        | ProbeRoundCompletionReason::AllProbesTerminal
                )
        }
        ProbeRoundReceiptStatus::Failed => {
            receipt.error.is_some()
                && matches!(
                    receipt.completion_reason,
                    ProbeRoundCompletionReason::AllProbesTerminal
                        | ProbeRoundCompletionReason::BudgetExhausted
                        | ProbeRoundCompletionReason::InfrastructureError
                )
                && (!matches!(
                    receipt.completion_reason,
                    ProbeRoundCompletionReason::BudgetExhausted
                ) || error_is(receipt.error.as_ref(), &DebugProbeErrorCode::BudgetExceeded))
                && (!matches!(
                    receipt.completion_reason,
                    ProbeRoundCompletionReason::AllProbesTerminal
                ) || !completion_satisfied
                    || required_failure)
        }
        ProbeRoundReceiptStatus::Cancelled => {
            matches!(
                receipt.completion_reason,
                ProbeRoundCompletionReason::Cancelled
            ) && error_is(receipt.error.as_ref(), &DebugProbeErrorCode::Cancelled)
        }
        ProbeRoundReceiptStatus::Stale => {
            matches!(
                receipt.completion_reason,
                ProbeRoundCompletionReason::StaleAuthority
            ) && error_is(receipt.error.as_ref(), &DebugProbeErrorCode::StaleAuthority)
        }
    }
}

fn probe_identity(
    authority: &DebugProbeRoundAuthority,
    probe: &ValidatedProbe,
) -> DebugProbeIdentity {
    DebugProbeIdentity {
        attempt: authority.attempt,
        debug_session_id: authority.debug_session_id.clone(),
        environment_digest: authority.environment_digest.clone(),
        fencing_token: authority.fencing_token.clone(),
        job_id: authority.job_id.clone(),
        lease_id: authority.lease_id.clone(),
        probe_execution_id: probe.execution_id.clone(),
        probe_id: probe.spec.probe_id.clone(),
        repository_id: authority.repository_id.clone(),
        round_id: authority.round_id.clone(),
        session_identity: authority.session_identity.clone(),
        workspace_revision: authority.workspace_revision.clone(),
    }
}

fn validate_plan_shape(plan: &DebugProbePlan) -> Result<(), DebugProbeContractError> {
    if plan.schema_version != 1
        || plan.probes.is_empty()
        || plan.probes.len() > 32
        || !canonical_instant(&plan.created_at.0)
        || !is_sha256_digest(&plan.plan_digest.0)
    {
        return Err(invalid_plan("probe plan shape is invalid"));
    }
    validate_authority_shape(&plan.authority)?;
    validate_budget_shape(&plan.budget)?;
    validate_completion_rule_shape(&plan.completion_rule)?;
    for probe in &plan.probes {
        validate_probe_spec_shape(probe)?;
    }
    Ok(())
}

fn validate_probe_spec_shape(spec: &ProbeSpec) -> Result<(), DebugProbeContractError> {
    if !prefixed_ulid(&spec.probe_id.0, "prb_")
        || !is_sha256_digest(&spec.probe_definition_digest.0)
        || spec.command.argv.is_empty()
        || spec.command.argv.len() > 64
        || !canonical_relative_path(&spec.command.working_directory)
        || !(1..=262_144).contains(&spec.command.command_arg_bytes)
        || !(1..=600_000).contains(&spec.timeout_millis)
        || !(1..=16_777_216).contains(&spec.output_limit_bytes)
        || spec.target_hypothesis_ids.is_empty()
        || spec.target_hypothesis_ids.len() > 16
        || spec
            .target_hypothesis_ids
            .iter()
            .any(|identity| !prefixed_ulid(&identity.0, "hyp_"))
        || !unique_strings(
            spec.target_hypothesis_ids
                .iter()
                .map(|identity| identity.0.as_str()),
        )
    {
        return Err(invalid_probe("probe specification shape is invalid"));
    }
    derive_probe_command_arg_bytes(&spec.command.argv)?;
    validate_resource_claim(&spec.resources)
}

fn validate_resource_claim(claim: &ProbeResourceClaim) -> Result<(), DebugProbeContractError> {
    if claim.paths.len() > 128
        || claim
            .paths
            .iter()
            .any(|path| !canonical_relative_path(path))
        || !unique_strings(claim.paths.iter().map(String::as_str))
        || !(1..=3_600_000).contains(&claim.cpu_limit_millis)
        || !(1_048_576..=8_589_934_592).contains(&claim.memory_limit_bytes)
        || claim.port_numbers.len() > 32
        || claim
            .port_numbers
            .iter()
            .any(|port| !(1..=65_535).contains(port))
        || !unique_i64(&claim.port_numbers)
        || !valid_resource_keys(&claim.service_keys, 32)
        || !valid_resource_keys(&claim.database_keys, 16)
        || !valid_resource_keys(&claim.exclusive_keys, 16)
        || !matches!(claim.workspace_access, ProbeWorkspaceAccess::ReadOnly)
    {
        return Err(invalid_probe("probe resource claim shape is invalid"));
    }
    Ok(())
}

fn validate_budget_shape(budget: &ProbeRoundBudget) -> Result<(), DebugProbeContractError> {
    if !(1..=32).contains(&budget.probe_limit)
        || !(1..=16).contains(&budget.parallel_probe_limit)
        || !(1..=3_600_000).contains(&budget.wall_time_limit_millis)
        || !(1..=268_435_456).contains(&budget.total_output_limit_bytes)
        || !(1..=28_800_000).contains(&budget.total_cpu_limit_millis)
        || !(1_048_576..=34_359_738_368).contains(&budget.peak_memory_limit_bytes)
        || !(1..=8_388_608).contains(&budget.total_command_arg_limit_bytes)
        || !is_sha256_digest(&budget.budget_digest.0)
    {
        return Err(invalid_plan("probe round budget shape is invalid"));
    }
    Ok(())
}

fn validate_completion_rule_shape(
    rule: &ProbeCompletionRule,
) -> Result<(), DebugProbeContractError> {
    if !(1..=32).contains(&rule.minimum_completed_probes)
        || !(0..=32).contains(&rule.minimum_successful_probes)
    {
        return Err(invalid_plan("probe completion rule shape is invalid"));
    }
    Ok(())
}

fn validate_authority_shape(
    authority: &DebugProbeRoundAuthority,
) -> Result<(), DebugProbeContractError> {
    let session = &authority.session_identity;
    if !prefixed_ulid(&authority.debug_session_id.0, "dbg_")
        || !prefixed_ulid(&authority.job_id.0, "job_")
        || !(1..=1_000).contains(&authority.attempt)
        || !prefixed_ulid(&authority.lease_id.0, "lse_")
        || !canonical_fencing_token(&authority.fencing_token.0)
        || !prefixed_ulid(&session.product_session_id.0, "psn_")
        || !prefixed_ulid(&session.worker_session_id.0, "wsn_")
        || !prefixed_ulid(&session.codex_thread_id.0, "cdx_")
        || session
            .work_run_id
            .as_ref()
            .is_some_and(|identity| !prefixed_ulid(&identity.0, "wrn_"))
        || !prefixed_ulid(&authority.repository_id.0, "rep_")
        || !prefixed_ulid(&authority.round_id.0, "prn_")
        || !canonical_workspace_revision(&authority.workspace_revision.0)
        || !is_sha256_digest(&authority.environment_digest.0)
    {
        return Err(stale_authority("DebugProbe round authority is invalid"));
    }
    Ok(())
}

fn hash_round_authority(
    digest: &mut FramedDigest,
    authority: &DebugProbeRoundAuthority,
) -> Result<(), DebugProbeContractError> {
    validate_authority_shape(authority)?;
    digest.text(&authority.debug_session_id.0)?;
    digest.text(&authority.job_id.0)?;
    digest.i64(authority.attempt);
    digest.text(&authority.lease_id.0)?;
    digest.text(&authority.fencing_token.0)?;
    digest.text(&authority.session_identity.product_session_id.0)?;
    digest.optional_text(
        authority
            .session_identity
            .work_run_id
            .as_ref()
            .map(|identity| identity.0.as_str()),
    )?;
    digest.text(&authority.session_identity.worker_session_id.0)?;
    digest.text(&authority.session_identity.codex_thread_id.0)?;
    digest.text(&authority.repository_id.0)?;
    digest.text(&authority.round_id.0)?;
    digest.text(&authority.workspace_revision.0)?;
    digest.text(&authority.environment_digest.0)
}

fn hash_resource_claim(
    digest: &mut FramedDigest,
    claim: &ProbeResourceClaim,
) -> Result<(), DebugProbeContractError> {
    validate_resource_claim(claim)?;
    digest.text(match claim.workspace_access {
        ProbeWorkspaceAccess::ReadOnly => "read_only",
    })?;
    digest.text(side_effect_tag(&claim.side_effect_class))?;
    hash_sorted_text(digest, &claim.paths)?;
    digest.i64(claim.cpu_limit_millis);
    digest.i64(claim.memory_limit_bytes);
    let mut ports = claim.port_numbers.clone();
    ports.sort_unstable();
    digest.list_len(ports.len())?;
    for port in ports {
        digest.i64(port);
    }
    hash_sorted_text(digest, &claim.service_keys)?;
    hash_sorted_text(digest, &claim.database_keys)?;
    hash_sorted_text(digest, &claim.exclusive_keys)?;
    digest.text(network_tag(&claim.network_access))
}

fn hash_completion_rule(
    digest: &mut FramedDigest,
    rule: &ProbeCompletionRule,
) -> Result<(), DebugProbeContractError> {
    validate_completion_rule_shape(rule)?;
    digest.text(match rule.kind {
        ProbeCompletionRuleKind::AllTerminal => "all_terminal",
        ProbeCompletionRuleKind::MinimumSuccesses => "minimum_successes",
        ProbeCompletionRuleKind::FirstConclusive => "first_conclusive",
    })?;
    digest.i64(rule.minimum_completed_probes);
    digest.i64(rule.minimum_successful_probes);
    digest.boolean(rule.stop_on_required_probe_failure);
    Ok(())
}

fn hash_sorted_text(
    digest: &mut FramedDigest,
    values: &[String],
) -> Result<(), DebugProbeContractError> {
    let mut sorted = values.iter().map(String::as_str).collect::<Vec<_>>();
    sorted.sort_unstable();
    digest.list_len(sorted.len())?;
    for value in sorted {
        digest.text(value)?;
    }
    Ok(())
}

fn checked_sum(left: i64, right: i64) -> Result<i64, DebugProbeContractError> {
    left.checked_add(right)
        .ok_or_else(|| budget_exceeded("probe budget arithmetic overflowed"))
}

fn validate_error_shape(error: Option<&DebugProbeError>) -> Result<(), DebugProbeContractError> {
    if error.is_some_and(|error| {
        error.message.is_empty()
            || error.message.chars().count() > 500
            || error.message.contains('\0')
    }) {
        return Err(invalid_probe("DebugProbe error shape is invalid"));
    }
    Ok(())
}

fn error_is(error: Option<&DebugProbeError>, expected: &DebugProbeErrorCode) -> bool {
    error.is_some_and(|error| error.code == *expected)
}

fn terminal_probe_status(status: &ProbeExecutionStatus) -> bool {
    matches!(
        status,
        ProbeExecutionStatus::Succeeded
            | ProbeExecutionStatus::Failed
            | ProbeExecutionStatus::TimedOut
            | ProbeExecutionStatus::Cancelled
            | ProbeExecutionStatus::Skipped
            | ProbeExecutionStatus::CacheHit
            | ProbeExecutionStatus::Stale
    )
}

fn event_status_matches_receipt(
    event: &ProbeExecutionStatus,
    receipt: &ProbeReceiptStatus,
) -> bool {
    matches!(
        (event, receipt),
        (
            ProbeExecutionStatus::Succeeded,
            ProbeReceiptStatus::Succeeded
        ) | (ProbeExecutionStatus::Failed, ProbeReceiptStatus::Failed)
            | (ProbeExecutionStatus::TimedOut, ProbeReceiptStatus::TimedOut)
            | (
                ProbeExecutionStatus::Cancelled,
                ProbeReceiptStatus::Cancelled
            )
            | (ProbeExecutionStatus::Skipped, ProbeReceiptStatus::Skipped)
            | (ProbeExecutionStatus::CacheHit, ProbeReceiptStatus::CacheHit)
            | (ProbeExecutionStatus::Stale, ProbeReceiptStatus::Stale)
    )
}

fn round_event_status_matches_receipt(
    event: &ProbeRoundStatus,
    receipt: &ProbeRoundReceiptStatus,
) -> bool {
    matches!(
        (event, receipt),
        (
            ProbeRoundStatus::Completed,
            ProbeRoundReceiptStatus::Completed
        ) | (ProbeRoundStatus::Failed, ProbeRoundReceiptStatus::Failed)
            | (
                ProbeRoundStatus::Cancelled,
                ProbeRoundReceiptStatus::Cancelled
            )
            | (ProbeRoundStatus::Stale, ProbeRoundReceiptStatus::Stale)
    )
}

fn unique_artifacts(values: &[crate::generated::ArtifactReference]) -> bool {
    let mut identities = HashSet::with_capacity(values.len());
    values.iter().all(|reference| {
        is_sha256_digest(&reference.digest.0)
            && prefixed_ulid(&reference.artifact_id.0, "art_")
            && identities.insert(reference.artifact_id.0.as_str())
    })
}

fn bounded_summary(value: &str) -> bool {
    !value.is_empty()
        && value.chars().count() <= 500
        && !value
            .chars()
            .any(|character| matches!(character, '\0' | '\r' | '\n'))
}

fn valid_resource_keys(values: &[String], maximum: usize) -> bool {
    values.len() <= maximum
        && unique_strings(values.iter().map(String::as_str))
        && values.iter().all(|value| {
            value.chars().count() <= 200
                && value
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_alphanumeric)
                && value.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric()
                        || matches!(byte, b'.' | b'_' | b':' | b'/' | b'@' | b'-')
                })
        })
}

fn unique_strings<'a>(values: impl IntoIterator<Item = &'a str>) -> bool {
    let mut unique = HashSet::new();
    values.into_iter().all(|value| unique.insert(value))
}

fn unique_i64(values: &[i64]) -> bool {
    let mut unique = HashSet::with_capacity(values.len());
    values.iter().all(|value| unique.insert(*value))
}

fn canonical_relative_path(value: &str) -> bool {
    if value == "." {
        return true;
    }
    !value.is_empty()
        && value.chars().count() <= 4_096
        && !value.starts_with('/')
        && !value.ends_with('/')
        && !value.contains('\0')
        && !value.contains('\\')
        && value
            .split('/')
            .all(|component| !component.is_empty() && !matches!(component, "." | ".."))
}

fn canonical_instant(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 24
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'T'
        && bytes[13] == b':'
        && bytes[16] == b':'
        && bytes[19] == b'.'
        && bytes[23] == b'Z'
        && bytes
            .iter()
            .enumerate()
            .filter(|(index, _)| !matches!(index, 4 | 7 | 10 | 13 | 16 | 19 | 23))
            .all(|(_, byte)| byte.is_ascii_digit())
        && (b'0'..=b'1').contains(&bytes[5])
        && (bytes[5] != b'0' || (b'1'..=b'9').contains(&bytes[6]))
        && (bytes[5] != b'1' || (b'0'..=b'2').contains(&bytes[6]))
        && (b'0'..=b'3').contains(&bytes[8])
        && (bytes[8] != b'3' || (b'0'..=b'1').contains(&bytes[9]))
        && (b'0'..=b'2').contains(&bytes[11])
        && (bytes[11] != b'2' || (b'0'..=b'3').contains(&bytes[12]))
        && (b'0'..=b'5').contains(&bytes[14])
        && (b'0'..=b'5').contains(&bytes[17])
}

fn prefixed_ulid(value: &str, prefix: &str) -> bool {
    value.len() == prefix.len() + 26
        && value.starts_with(prefix)
        && value[prefix.len()..].bytes().all(|byte| {
            byte.is_ascii_digit()
                || matches!(byte, b'A'..=b'H' | b'J'..=b'N' | b'P'..=b'T' | b'V'..=b'Z')
        })
}

fn canonical_fencing_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 20
        && value.as_bytes()[0].is_ascii_digit()
        && value.as_bytes()[0] != b'0'
        && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn canonical_workspace_revision(value: &str) -> bool {
    value.strip_prefix("git-tree:").is_some_and(|digest| {
        matches!(digest.len(), 40 | 64)
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    })
}

fn is_sha256_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|digest| {
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    })
}

fn require_digest(
    digest: &Sha256Digest,
    message: &'static str,
) -> Result<(), DebugProbeContractError> {
    if !is_sha256_digest(&digest.0) {
        return Err(invalid_plan(message));
    }
    Ok(())
}

const fn probe_kind_tag(kind: &DebugProbeKind) -> &'static str {
    match kind {
        DebugProbeKind::Search => "search",
        DebugProbeKind::Read => "read",
        DebugProbeKind::StaticAnalysis => "static_analysis",
        DebugProbeKind::Test => "test",
        DebugProbeKind::Build => "build",
        DebugProbeKind::RuntimeTrace => "runtime_trace",
        DebugProbeKind::DatabaseQuery => "database_query",
        DebugProbeKind::NetworkObservation => "network_observation",
    }
}

const fn side_effect_tag(value: &ProbeSideEffectClass) -> &'static str {
    match value {
        ProbeSideEffectClass::PureRead => "pure_read",
        ProbeSideEffectClass::IsolatedSideEffect => "isolated_side_effect",
        ProbeSideEffectClass::Exclusive => "exclusive",
    }
}

const fn network_tag(value: &ProbeNetworkAccess) -> &'static str {
    match value {
        ProbeNetworkAccess::None => "none",
        ProbeNetworkAccess::Loopback => "loopback",
        ProbeNetworkAccess::DeclaredReadOnly => "declared_read_only",
    }
}

fn invalid_plan(message: &'static str) -> DebugProbeContractError {
    DebugProbeContractError {
        code: DebugProbeErrorCode::InvalidPlan,
        message,
    }
}

fn invalid_probe(message: &'static str) -> DebugProbeContractError {
    DebugProbeContractError {
        code: DebugProbeErrorCode::InvalidProbe,
        message,
    }
}

fn budget_exceeded(message: &'static str) -> DebugProbeContractError {
    DebugProbeContractError {
        code: DebugProbeErrorCode::BudgetExceeded,
        message,
    }
}

fn stale_authority(message: &'static str) -> DebugProbeContractError {
    DebugProbeContractError {
        code: DebugProbeErrorCode::StaleAuthority,
        message,
    }
}

struct FramedDigest(Sha256);

impl FramedDigest {
    fn new(domain: &[u8]) -> Self {
        let mut digest = Sha256::new();
        digest.update(
            u64::try_from(domain.len())
                .expect("DebugProbe digest domain length fits u64")
                .to_be_bytes(),
        );
        digest.update(domain);
        Self(digest)
    }

    fn bytes(&mut self, value: &[u8]) -> Result<(), DebugProbeContractError> {
        let length = u64::try_from(value.len())
            .map_err(|_| invalid_plan("DebugProbe digest component is too large"))?;
        self.0.update(length.to_be_bytes());
        self.0.update(value);
        Ok(())
    }

    fn text(&mut self, value: &str) -> Result<(), DebugProbeContractError> {
        self.bytes(value.as_bytes())
    }

    fn i64(&mut self, value: i64) {
        self.0.update(8_u64.to_be_bytes());
        self.0.update(value.to_be_bytes());
    }

    fn boolean(&mut self, value: bool) {
        self.0.update(1_u64.to_be_bytes());
        self.0.update([u8::from(value)]);
    }

    fn list_len(&mut self, value: usize) -> Result<(), DebugProbeContractError> {
        let value = u64::try_from(value)
            .map_err(|_| invalid_plan("DebugProbe list length is too large"))?;
        self.0.update(8_u64.to_be_bytes());
        self.0.update(value.to_be_bytes());
        Ok(())
    }

    fn optional_text(&mut self, value: Option<&str>) -> Result<(), DebugProbeContractError> {
        self.boolean(value.is_some());
        if let Some(value) = value {
            self.text(value)?;
        }
        Ok(())
    }

    fn finish(self) -> Sha256Digest {
        Sha256Digest(format!("sha256:{:x}", self.0.finalize()))
    }
}
