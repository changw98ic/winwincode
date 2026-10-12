// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeSet;
use std::fmt;

use serde::Serialize;
use sha2::{Digest as _, Sha256};
use winwincode_domain::Sha256Digest;

use crate::generated::{
    ChangeBatchReceiptStatus, FinalCandidateFreezeFact, ObservationDecision, RepairLoopBudget,
    RepairLoopContextPack, RepairLoopCounters,
};

const HASH_DOMAIN: &[u8] = b"winwincode.repair-loop-context.v1\0";
const MAX_SERIALIZED_BYTES: usize = 131_072;

pub fn validate_repair_loop_budget(budget: &RepairLoopBudget) -> Result<(), RepairLoopBoundsError> {
    if budget.max_repair_rounds != 3
        || budget
            .max_observer_calls
            .is_some_and(|value| !(1..=4).contains(&value))
        || budget
            .max_primary_model_calls
            .is_some_and(|value| !(1..=8).contains(&value))
        || budget
            .max_total_tokens
            .is_some_and(|value| !(1..=10_000_000).contains(&value))
        || budget
            .max_total_cost_microunits
            .is_some_and(|value| !(1..=9_007_199_254_740_991).contains(&value))
        || budget
            .max_wall_time_millis
            .is_some_and(|value| !(1_000..=3_600_000).contains(&value))
        || !(1..=4).contains(&budget.max_change_batches)
        || !(1_024..=131_072).contains(&budget.max_context_pack_bytes)
    {
        return Err(RepairLoopBoundsError::InvalidBudget);
    }
    Ok(())
}

pub fn validate_repair_loop_counters(
    counters: &RepairLoopCounters,
) -> Result<(), RepairLoopBoundsError> {
    if !(0..=3).contains(&counters.repair_rounds)
        || !(0..=9_007_199_254_740_991).contains(&counters.observer_calls)
        || !(0..=9_007_199_254_740_991).contains(&counters.primary_model_calls)
        || !(0..=9_007_199_254_740_991).contains(&counters.total_tokens)
        || !(0..=9_007_199_254_740_991).contains(&counters.total_cost_microunits)
        || !(0..=9_007_199_254_740_991).contains(&counters.elapsed_millis)
        || !(0..=4).contains(&counters.change_batches)
        || !(0..=131_072).contains(&counters.context_pack_bytes)
    {
        return Err(RepairLoopBoundsError::InvalidCounters);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepairLoopBoundsError {
    InvalidBudget,
    InvalidCounters,
}

impl fmt::Display for RepairLoopBoundsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidBudget => "repair-loop budget is outside canonical bounds",
            Self::InvalidCounters => "repair-loop counters are outside canonical bounds",
        })
    }
}

impl std::error::Error for RepairLoopBoundsError {}

pub fn derive_repair_loop_context_digest(
    pack: &RepairLoopContextPack,
) -> Result<Sha256Digest, RepairLoopContextError> {
    let mut digest = Sha256::new();
    digest.update(HASH_DOMAIN);
    update_json_field(&mut digest, b"schemaVersion", &pack.schema_version)?;
    update_json_field(&mut digest, b"identity", &pack.identity)?;
    update_json_field(&mut digest, b"observedRevision", &pack.observed_revision)?;
    update_json_field(
        &mut digest,
        b"proposalDisposition",
        &pack.proposal_disposition,
    )?;
    update_json_field(&mut digest, b"goalSummary", &pack.goal_summary)?;
    update_json_field(
        &mut digest,
        b"completedAcceptanceCriteria",
        &pack.completed_acceptance_criteria,
    )?;
    update_json_field(
        &mut digest,
        b"incompleteAcceptanceCriteria",
        &pack.incomplete_acceptance_criteria,
    )?;
    update_json_field(&mut digest, b"repairEnvelope", &pack.repair_envelope)?;
    update_json_field(&mut digest, b"latestReceipt", &pack.latest_receipt)?;
    update_json_field(&mut digest, b"latestObservation", &pack.latest_observation)?;
    update_json_field(&mut digest, b"artifactRefs", &pack.artifact_refs)?;
    Ok(Sha256Digest(format!("sha256:{:x}", digest.finalize())))
}

pub fn seal_repair_loop_context_pack(
    mut pack: RepairLoopContextPack,
) -> Result<(RepairLoopContextPack, Vec<u8>), RepairLoopContextError> {
    validate_relationships(&pack)?;
    pack.context_digest = derive_repair_loop_context_digest(&pack)?;
    pack.serialized_byte_count = 0;

    for _ in 0..4 {
        let payload = serialize_pack(&pack)?;
        enforce_payload_limit(payload.len())?;
        let byte_count =
            i64::try_from(payload.len()).map_err(|_| RepairLoopContextError::PayloadTooLarge)?;
        if pack.serialized_byte_count == byte_count {
            return Ok((pack, payload));
        }
        pack.serialized_byte_count = byte_count;
    }

    Err(RepairLoopContextError::SerializedByteCountDidNotConverge)
}

pub fn validate_repair_loop_context_pack(
    pack: &RepairLoopContextPack,
    payload: &[u8],
) -> Result<(), RepairLoopContextError> {
    enforce_payload_limit(payload.len())?;
    let actual_byte_count =
        i64::try_from(payload.len()).map_err(|_| RepairLoopContextError::PayloadTooLarge)?;
    if pack.serialized_byte_count != actual_byte_count {
        return Err(RepairLoopContextError::SerializedByteCountMismatch);
    }

    let decoded: RepairLoopContextPack =
        serde_json::from_slice(payload).map_err(|_| RepairLoopContextError::InvalidPayload)?;
    if &decoded != pack || serialize_pack(pack)? != payload {
        return Err(RepairLoopContextError::NonCanonicalPayload);
    }

    validate_relationships(pack)?;
    if derive_repair_loop_context_digest(pack)? != pack.context_digest {
        return Err(RepairLoopContextError::DigestMismatch);
    }
    Ok(())
}

pub fn validate_final_candidate_freeze_fact(
    fact: &FinalCandidateFreezeFact,
) -> Result<(), FinalCandidateFreezeError> {
    if fact.schema_version != 1 {
        return Err(FinalCandidateFreezeError::InvalidSchemaVersion);
    }
    if fact.final_receipt.identity != fact.identity {
        return Err(FinalCandidateFreezeError::IdentityMismatch);
    }
    if fact.final_receipt.status != ChangeBatchReceiptStatus::Applied
        || !fact.final_receipt.delta_exact
    {
        return Err(FinalCandidateFreezeError::InexactReceipt);
    }
    if fact.final_receipt.result_revision.as_ref() != Some(&fact.result_revision) {
        return Err(FinalCandidateFreezeError::RevisionMismatch);
    }
    if fact.final_receipt.delta_digest.as_ref() != Some(&fact.delta_digest) {
        return Err(FinalCandidateFreezeError::DeltaMismatch);
    }
    if !is_sha256_digest(&fact.delta_digest.0)
        || !is_sha256_digest(&fact.context_pack_digest.0)
        || !is_sha256_digest(&fact.candidate_artifact_ref.digest.0)
    {
        return Err(FinalCandidateFreezeError::InvalidDigest);
    }
    if let Some(observation) = &fact.final_observation {
        if observation.identity != fact.identity {
            return Err(FinalCandidateFreezeError::IdentityMismatch);
        }
        if observation.result_revision != fact.result_revision {
            return Err(FinalCandidateFreezeError::RevisionMismatch);
        }
        if observation.response.decision != ObservationDecision::Accept {
            return Err(FinalCandidateFreezeError::ObservationNotAccepted);
        }
    }
    if let Some(receipt_observation) = &fact.final_receipt.observation
        && fact.final_observation.as_ref() != Some(receipt_observation)
    {
        return Err(FinalCandidateFreezeError::ObservationMismatch);
    }
    if !counters_within_canonical_limits(fact) {
        return Err(FinalCandidateFreezeError::CounterLimitExceeded);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FinalCandidateFreezeError {
    InvalidSchemaVersion,
    IdentityMismatch,
    InexactReceipt,
    RevisionMismatch,
    DeltaMismatch,
    InvalidDigest,
    ObservationNotAccepted,
    ObservationMismatch,
    CounterLimitExceeded,
}

impl fmt::Display for FinalCandidateFreezeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidSchemaVersion => "final-candidate schema version is invalid",
            Self::IdentityMismatch => "final-candidate identity binding does not match",
            Self::InexactReceipt => "final-candidate receipt is not an exact applied result",
            Self::RevisionMismatch => "final-candidate result revision does not match",
            Self::DeltaMismatch => "final-candidate delta does not match",
            Self::InvalidDigest => "final-candidate digest is invalid",
            Self::ObservationNotAccepted => "final-candidate observation did not accept",
            Self::ObservationMismatch => "final-candidate observations do not match",
            Self::CounterLimitExceeded => "final-candidate loop counter exceeds its limit",
        })
    }
}

impl std::error::Error for FinalCandidateFreezeError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepairLoopContextError {
    Serialization,
    InvalidPayload,
    NonCanonicalPayload,
    PayloadTooLarge,
    SerializedByteCountMismatch,
    SerializedByteCountDidNotConverge,
    InvalidSchemaVersion,
    InvalidGoalSummary,
    AcceptanceCriteriaLimitExceeded,
    MissingAcceptanceCriteria,
    InvalidAcceptanceCriterion,
    DuplicateAcceptanceCriterion,
    ArtifactReferenceLimitExceeded,
    IdentityMismatch,
    RevisionMismatch,
    InexactLatestReceipt,
    InvalidRepairRound,
    DigestMismatch,
}

impl fmt::Display for RepairLoopContextError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Serialization => "repair-loop context serialization failed",
            Self::InvalidPayload => "repair-loop context payload is invalid",
            Self::NonCanonicalPayload => "repair-loop context payload is not canonical JSON",
            Self::PayloadTooLarge => "repair-loop context payload exceeds 131072 bytes",
            Self::SerializedByteCountMismatch => {
                "repair-loop context serialized byte count does not match the payload"
            }
            Self::SerializedByteCountDidNotConverge => {
                "repair-loop context serialized byte count did not converge"
            }
            Self::InvalidSchemaVersion => "repair-loop context schema version is invalid",
            Self::InvalidGoalSummary => "repair-loop context goal summary is invalid",
            Self::AcceptanceCriteriaLimitExceeded => {
                "repair-loop context acceptance-criterion limit is exceeded"
            }
            Self::MissingAcceptanceCriteria => "repair-loop context has no acceptance criteria",
            Self::InvalidAcceptanceCriterion => {
                "repair-loop context acceptance criterion is invalid"
            }
            Self::DuplicateAcceptanceCriterion => {
                "repair-loop context repeats an acceptance criterion"
            }
            Self::ArtifactReferenceLimitExceeded => {
                "repair-loop context Artifact reference limit is exceeded"
            }
            Self::IdentityMismatch => "repair-loop context identity binding does not match",
            Self::RevisionMismatch => "repair-loop context revision binding does not match",
            Self::InexactLatestReceipt => {
                "repair-loop context latest receipt has no exact result revision"
            }
            Self::InvalidRepairRound => "repair-loop context repair round is outside 1 through 3",
            Self::DigestMismatch => "repair-loop context digest does not match",
        })
    }
}

impl std::error::Error for RepairLoopContextError {}

fn validate_relationships(pack: &RepairLoopContextPack) -> Result<(), RepairLoopContextError> {
    if pack.schema_version != 1 {
        return Err(RepairLoopContextError::InvalidSchemaVersion);
    }
    if !valid_summary(&pack.goal_summary) {
        return Err(RepairLoopContextError::InvalidGoalSummary);
    }
    if pack.completed_acceptance_criteria.len() > 64
        || pack.incomplete_acceptance_criteria.len() > 64
    {
        return Err(RepairLoopContextError::AcceptanceCriteriaLimitExceeded);
    }
    if pack.completed_acceptance_criteria.is_empty()
        && pack.incomplete_acceptance_criteria.is_empty()
    {
        return Err(RepairLoopContextError::MissingAcceptanceCriteria);
    }

    let mut criterion_ids = BTreeSet::new();
    for criterion in pack
        .completed_acceptance_criteria
        .iter()
        .chain(&pack.incomplete_acceptance_criteria)
    {
        if !valid_token(&criterion.id) || !valid_summary(&criterion.summary) {
            return Err(RepairLoopContextError::InvalidAcceptanceCriterion);
        }
        if !criterion_ids.insert(criterion.id.as_str()) {
            return Err(RepairLoopContextError::DuplicateAcceptanceCriterion);
        }
    }
    if pack.artifact_refs.len() > 16 {
        return Err(RepairLoopContextError::ArtifactReferenceLimitExceeded);
    }

    if pack.latest_receipt.identity != pack.identity {
        return Err(RepairLoopContextError::IdentityMismatch);
    }
    if pack.latest_receipt.status != ChangeBatchReceiptStatus::Applied
        || !pack.latest_receipt.delta_exact
    {
        return Err(RepairLoopContextError::InexactLatestReceipt);
    }
    match pack.latest_receipt.result_revision.as_ref() {
        Some(revision) if revision == &pack.observed_revision => {}
        Some(_) => return Err(RepairLoopContextError::RevisionMismatch),
        None => return Err(RepairLoopContextError::InexactLatestReceipt),
    }

    if let Some(observation) = &pack.latest_observation {
        if observation.identity != pack.identity {
            return Err(RepairLoopContextError::IdentityMismatch);
        }
        if observation.result_revision != pack.observed_revision {
            return Err(RepairLoopContextError::RevisionMismatch);
        }
    }

    if let Some(repair) = &pack.repair_envelope {
        if repair.identity != pack.identity {
            return Err(RepairLoopContextError::IdentityMismatch);
        }
        if repair.observed_revision != pack.observed_revision {
            return Err(RepairLoopContextError::RevisionMismatch);
        }
        if !(1..=3).contains(&repair.repair_round) {
            return Err(RepairLoopContextError::InvalidRepairRound);
        }
    }

    Ok(())
}

fn update_json_field<T: Serialize>(
    digest: &mut Sha256,
    field_name: &[u8],
    value: &T,
) -> Result<(), RepairLoopContextError> {
    let encoded = serde_json::to_vec(value).map_err(|_| RepairLoopContextError::Serialization)?;
    update_framed(digest, field_name);
    update_framed(digest, &encoded);
    Ok(())
}

fn update_framed(digest: &mut Sha256, value: &[u8]) {
    let length = u64::try_from(value.len()).expect("bounded context field length fits u64");
    digest.update(length.to_be_bytes());
    digest.update(value);
}

fn serialize_pack(pack: &RepairLoopContextPack) -> Result<Vec<u8>, RepairLoopContextError> {
    serde_json::to_vec(pack).map_err(|_| RepairLoopContextError::Serialization)
}

fn enforce_payload_limit(length: usize) -> Result<(), RepairLoopContextError> {
    if length > MAX_SERIALIZED_BYTES {
        return Err(RepairLoopContextError::PayloadTooLarge);
    }
    Ok(())
}

fn counters_within_canonical_limits(fact: &FinalCandidateFreezeFact) -> bool {
    validate_repair_loop_counters(&fact.counters).is_ok()
}

fn valid_summary(value: &str) -> bool {
    !value.is_empty()
        && value.chars().count() <= 500
        && !value
            .chars()
            .any(|character| matches!(character, '\0' | '\r' | '\n'))
}

fn valid_token(value: &str) -> bool {
    if value.is_empty() || value.len() > 200 {
        return false;
    }
    let mut bytes = value.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && bytes.all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'/' | b'@' | b'-')
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
