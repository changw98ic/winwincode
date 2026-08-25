// SPDX-License-Identifier: Apache-2.0

//! Public-output redaction for runtime and frozen-candidate projections.

use std::{collections::HashSet, error::Error, fmt};

use serde::Serialize;
use sha2::{Digest, Sha256};
use winwincode_domain::{DeliveryId, Sha256Digest};

use crate::domain::{
    CandidatePathFact, Delivery, FrozenDeliveryCandidate, StageRunStatus,
    assert_frozen_candidate_current, candidate::CandidateHunkFact,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectionRedactionErrorCode {
    InvalidSource,
    InvalidDetails,
    StaleCandidate,
    Unauthorized,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionRedactionError {
    code: ProjectionRedactionErrorCode,
    message: String,
}

impl ProjectionRedactionError {
    pub const fn code(&self) -> ProjectionRedactionErrorCode {
        self.code
    }
}

impl fmt::Display for ProjectionRedactionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for ProjectionRedactionError {}

/// Secret-safe live Diff aggregate. Its shape deliberately has no path, hunk,
/// or unified-Diff field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeDiffSummaryProjection {
    changed_file_count: u64,
    additions: u64,
    deletions: u64,
    details_visible: bool,
    source_ref: String,
}

impl RuntimeDiffSummaryProjection {
    pub const fn changed_file_count(&self) -> u64 {
        self.changed_file_count
    }

    pub const fn additions(&self) -> u64 {
        self.additions
    }

    pub const fn deletions(&self) -> u64 {
        self.deletions
    }

    pub const fn details_visible(&self) -> bool {
        self.details_visible
    }

    pub fn source_ref(&self) -> &str {
        &self.source_ref
    }
}

// Phase 4 owns the production accepted-runtime adapter. Until then only the
// feature-gated fixture adapter can mint this summary.
#[allow(dead_code)]
pub(crate) fn live_diff_summary(
    changed_file_count: u64,
    additions: u64,
    deletions: u64,
    source_ref: &str,
) -> Result<RuntimeDiffSummaryProjection, ProjectionRedactionError> {
    const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
    if [changed_file_count, additions, deletions]
        .into_iter()
        .any(|value| value > MAX_SAFE_INTEGER)
        || !is_safe_source_ref(source_ref)
    {
        return Err(redaction_error(
            ProjectionRedactionErrorCode::InvalidSource,
            "live Diff summary requires safe counts and one opaque source reference",
        ));
    }
    Ok(RuntimeDiffSummaryProjection {
        changed_file_count,
        additions,
        deletions,
        details_visible: false,
        source_ref: source_ref.to_owned(),
    })
}

#[cfg(any(test, feature = "test-support"))]
#[allow(dead_code)]
fn summarize_live_diff(
    unified_diff: &str,
    source_ref: &str,
) -> Result<RuntimeDiffSummaryProjection, ProjectionRedactionError> {
    let mut changed_file_count = 0_u64;
    let mut additions = 0_u64;
    let mut deletions = 0_u64;
    for line in unified_diff.lines() {
        if line.starts_with("diff --git ") {
            changed_file_count = changed_file_count.saturating_add(1);
        } else if line.starts_with('+') && !line.starts_with("+++") {
            additions = additions.saturating_add(1);
        } else if line.starts_with('-') && !line.starts_with("---") {
            deletions = deletions.saturating_add(1);
        }
    }
    live_diff_summary(changed_file_count, additions, deletions, source_ref)
}

pub(crate) fn is_safe_source_ref(value: &str) -> bool {
    let normalized = value.to_ascii_lowercase();
    let mut bytes = value.bytes();
    value.len() <= 4_096
        && bytes
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && bytes.all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'.' | b'_' | b':' | b'/' | b'@' | b'-' | b'#')
        })
        && !value.contains("..")
        && !value.contains("//")
        && !normalized.starts_with("file:")
        && !normalized.contains("authorization=")
        && !normalized.contains("credential=")
        && !normalized.contains("secret=")
        && !normalized.contains("token=")
        && !contains_credential_material(&normalized)
}

pub(crate) fn contains_credential_material(value: &str) -> bool {
    let normalized = value.to_ascii_lowercase();
    let secret_markers = [
        "--api-key ",
        "--password ",
        "--secret ",
        "--token ",
        "api_key=",
        "api-key:",
        "apikey=",
        "authorization:",
        "authorization=",
        "aws_secret_access_key",
        "bearer ",
        "credential=",
        "gho_",
        "ghp_",
        "ghr_",
        "ghs_",
        "ghu_",
        "github_pat_",
        "glpat-",
        "-----begin private key-----",
        "-----begin rsa private key-----",
        "-----begin openssh private key-----",
        "password=",
        "sk-proj-",
        "sk-svcacct-",
        "secret=",
        "token=",
        "xapp-",
        "x-api-key:",
        "xoxb-",
        "xoxp-",
    ];
    secret_markers
        .iter()
        .any(|marker| normalized.contains(marker))
        || contains_url_userinfo(&normalized)
}

fn contains_url_userinfo(value: &str) -> bool {
    let mut remainder = value;
    while let Some(scheme_end) = remainder.find("://") {
        let after_scheme = &remainder[scheme_end + 3..];
        let authority_end = after_scheme
            .find(|character: char| character.is_ascii_whitespace() || "/?#".contains(character))
            .unwrap_or(after_scheme.len());
        let authority = &after_scheme[..authority_end];
        if authority
            .rfind('@')
            .is_some_and(|at| authority[..at].contains(':'))
        {
            return true;
        }
        remainder = &after_scheme[authority_end..];
    }
    false
}

fn redaction_error(
    code: ProjectionRedactionErrorCode,
    message: impl Into<String>,
) -> ProjectionRedactionError {
    ProjectionRedactionError {
        code,
        message: message.into(),
    }
}

/// Sealed Git-adapter detail fact for one already frozen candidate. Fields and
/// construction stay closed until the real Git source adapter exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptedFrozenCandidateDetails {
    candidate_ref: String,
    diff_sha256: String,
    changed_paths: Vec<CandidatePathFact>,
    changed_hunks: Vec<CandidateHunkFact>,
    source_ref: String,
    seal: Sha256Digest,
}

/// Capability proving one authenticated viewer may read the exact candidate.
/// It is intentionally not deserializable and has no public constructor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrozenCandidateReviewGrant {
    delivery_id: DeliveryId,
    delivery_spec_revision: u64,
    candidate_ref: String,
    diff_sha256: String,
    reviewer_id: String,
    can_review: bool,
    seal: Sha256Digest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FrozenCandidateDetailProjection {
    pub candidate_ref: String,
    pub diff_sha256: String,
    pub paths: Vec<CandidatePathFact>,
    pub hunks: Vec<CandidateHunkFact>,
    pub source_ref: String,
}

/// Produces the separately authorized path-and-hunk projection for one exact,
/// current, settled frozen candidate.
///
/// # Errors
///
/// Rejects stale candidates, modified adapter details, changed Diff identity,
/// incomplete path/hunk scope, and missing review permission.
pub fn project_frozen_candidate_details(
    delivery: &Delivery,
    candidate: &FrozenDeliveryCandidate,
    details: &AcceptedFrozenCandidateDetails,
    grant: &FrozenCandidateReviewGrant,
) -> Result<FrozenCandidateDetailProjection, ProjectionRedactionError> {
    assert_frozen_candidate_current(delivery, candidate).map_err(|_| {
        redaction_error(
            ProjectionRedactionErrorCode::StaleCandidate,
            "frozen candidate is not current for this Delivery",
        )
    })?;
    let producer = delivery
        .snapshot()
        .stage_runs
        .iter()
        .find(|run| run.id == *candidate.producer_stage_run_id())
        .ok_or_else(|| {
            redaction_error(
                ProjectionRedactionErrorCode::StaleCandidate,
                "frozen candidate producer StageRun is missing",
            )
        })?;
    if producer.status != StageRunStatus::Succeeded || producer.finished_at_millis.is_none() {
        return Err(redaction_error(
            ProjectionRedactionErrorCode::StaleCandidate,
            "frozen candidate producer StageRun is not settled successfully",
        ));
    }

    if grant.seal != seal_review_grant(grant)?
        || !grant.can_review
        || grant.delivery_id != *delivery.id()
        || grant.delivery_spec_revision != delivery.snapshot().spec.revision
        || grant.candidate_ref != candidate.candidate_ref()
        || grant.diff_sha256 != candidate.diff_sha256()
        || !is_safe_reviewer_id(&grant.reviewer_id)
    {
        return Err(redaction_error(
            ProjectionRedactionErrorCode::Unauthorized,
            "candidate details require one exact sealed Delivery review grant",
        ));
    }

    if details.seal != seal_candidate_details(details)?
        || details.candidate_ref != candidate.candidate_ref()
        || details.diff_sha256 != candidate.diff_sha256()
        || details.changed_paths != candidate.changed_paths()
        || !is_safe_source_ref(&details.source_ref)
        || !valid_hunks(candidate, &details.changed_hunks)
    {
        return Err(redaction_error(
            ProjectionRedactionErrorCode::InvalidDetails,
            "candidate detail fact does not match the exact frozen Diff",
        ));
    }

    let mut paths = details.changed_paths.clone();
    paths.sort_by(|left, right| left.path.cmp(&right.path));
    let mut hunks = details.changed_hunks.clone();
    hunks.sort_by(|left, right| {
        (&left.file_path, &left.hunk_sha256, &left.source_hunk_sha256).cmp(&(
            &right.file_path,
            &right.hunk_sha256,
            &right.source_hunk_sha256,
        ))
    });
    Ok(FrozenCandidateDetailProjection {
        candidate_ref: candidate.candidate_ref().to_owned(),
        diff_sha256: candidate.diff_sha256().to_owned(),
        paths,
        hunks,
        source_ref: details.source_ref.clone(),
    })
}

fn valid_hunks(candidate: &FrozenDeliveryCandidate, hunks: &[CandidateHunkFact]) -> bool {
    if hunks.len() > 100_000 {
        return false;
    }
    let paths = candidate
        .changed_paths()
        .iter()
        .map(|path| path.path.as_str())
        .collect::<HashSet<_>>();
    let mut identities = HashSet::new();
    hunks.iter().all(|hunk| {
        paths.contains(hunk.file_path.as_str())
            && sha256_hex(&hunk.hunk_sha256)
            && hunk.source_hunk_sha256.as_deref().is_none_or(sha256_hex)
            && identities.insert((
                hunk.file_path.as_str(),
                hunk.hunk_sha256.as_str(),
                hunk.source_hunk_sha256.as_deref(),
            ))
    })
}

fn sha256_hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn is_safe_reviewer_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 200
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'@' | b'-')
        })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CandidateDetailsSeal<'details> {
    candidate_ref: &'details str,
    diff_sha256: &'details str,
    changed_paths: &'details [CandidatePathFact],
    changed_hunks: &'details [CandidateHunkFact],
    source_ref: &'details str,
}

fn seal_candidate_details(
    details: &AcceptedFrozenCandidateDetails,
) -> Result<Sha256Digest, ProjectionRedactionError> {
    seal_value(&CandidateDetailsSeal {
        candidate_ref: &details.candidate_ref,
        diff_sha256: &details.diff_sha256,
        changed_paths: &details.changed_paths,
        changed_hunks: &details.changed_hunks,
        source_ref: &details.source_ref,
    })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ReviewGrantSeal<'grant> {
    delivery_id: &'grant DeliveryId,
    delivery_spec_revision: u64,
    candidate_ref: &'grant str,
    diff_sha256: &'grant str,
    reviewer_id: &'grant str,
    can_review: bool,
}

fn seal_review_grant(
    grant: &FrozenCandidateReviewGrant,
) -> Result<Sha256Digest, ProjectionRedactionError> {
    seal_value(&ReviewGrantSeal {
        delivery_id: &grant.delivery_id,
        delivery_spec_revision: grant.delivery_spec_revision,
        candidate_ref: &grant.candidate_ref,
        diff_sha256: &grant.diff_sha256,
        reviewer_id: &grant.reviewer_id,
        can_review: grant.can_review,
    })
}

fn seal_value(value: &impl Serialize) -> Result<Sha256Digest, ProjectionRedactionError> {
    let encoded = serde_json::to_vec(value).map_err(|error| {
        redaction_error(
            ProjectionRedactionErrorCode::InvalidDetails,
            format!("candidate detail seal cannot be encoded: {error}"),
        )
    })?;
    Ok(Sha256Digest(format!(
        "sha256:{:x}",
        Sha256::digest(encoded)
    )))
}

#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    use super::{
        AcceptedFrozenCandidateDetails, CandidateHunkFact, FrozenCandidateReviewGrant,
        FrozenDeliveryCandidate, Sha256Digest, seal_candidate_details, seal_review_grant,
    };

    /// Creates a sealed Git-adapter fixture for one frozen candidate.
    ///
    /// # Panics
    ///
    /// Panics only when the test fixture cannot be deterministically sealed.
    #[must_use]
    pub fn accepted_frozen_candidate_details(
        candidate: &FrozenDeliveryCandidate,
        changed_hunks: Vec<CandidateHunkFact>,
        source_ref: &str,
    ) -> AcceptedFrozenCandidateDetails {
        let mut details = AcceptedFrozenCandidateDetails {
            candidate_ref: candidate.candidate_ref().to_owned(),
            diff_sha256: candidate.diff_sha256().to_owned(),
            changed_paths: candidate.changed_paths().to_vec(),
            changed_hunks,
            source_ref: source_ref.to_owned(),
            seal: Sha256Digest(String::new()),
        };
        details.seal = seal_candidate_details(&details).expect("candidate detail fixture seal");
        details
    }

    /// Creates one sealed review-grant fixture. Production identity code does
    /// not enable this feature and therefore cannot forge the capability.
    ///
    /// # Panics
    ///
    /// Panics only when the test fixture cannot be deterministically sealed.
    #[must_use]
    pub fn candidate_review_grant(
        candidate: &FrozenDeliveryCandidate,
        reviewer_id: &str,
        can_review: bool,
    ) -> FrozenCandidateReviewGrant {
        let mut grant = FrozenCandidateReviewGrant {
            delivery_id: candidate.delivery_id().clone(),
            delivery_spec_revision: candidate.delivery_spec_revision(),
            candidate_ref: candidate.candidate_ref().to_owned(),
            diff_sha256: candidate.diff_sha256().to_owned(),
            reviewer_id: reviewer_id.to_owned(),
            can_review,
            seal: Sha256Digest(String::new()),
        };
        grant.seal = seal_review_grant(&grant).expect("candidate review fixture seal");
        grant
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        Delivery, DeliveryStage, DeliveryStatus, SessionBindingId, StageRun, StageRunActorType,
        StageRunId, StageRunStatus,
        candidate::{CandidateHunkFact, test_support::frozen_candidate},
        test_fixture,
    };
    use crate::projection::runtime::{
        RuntimeActivityOutcome, RuntimeActivityProjection, RuntimeActivityStatus,
        RuntimeActivityType, RuntimeProjection,
        test_support::{
            RuntimeAuthorityFixture, RuntimeFactFixture, accepted_binding, accepted_event,
        },
    };
    use winwincode_domain::{CodexThreadId, ExecutionJobId, ProductSessionId, WorkerSessionId};

    fn writer_delivery() -> Delivery {
        let mut snapshot = test_fixture();
        snapshot.status = DeliveryStatus::Verifying;
        snapshot.evidence.clear();
        snapshot.verdict = None;
        let run = &mut snapshot.stage_runs[0];
        run.id = StageRunId("stage-executor-details".into());
        run.stage = DeliveryStage::Executing;
        run.role = "executor".into();
        run.status = StageRunStatus::Succeeded;
        run.started_at_millis = 1_800_000_000_010;
        run.finished_at_millis = Some(1_800_000_000_020);
        let binding = &mut snapshot.session_bindings[0];
        binding.id = SessionBindingId("binding-executor-details".into());
        binding.stage_run_id = run.id.clone();
        binding.product_session_id = ProductSessionId("product-executor-details".into());
        binding.execution_job_id = ExecutionJobId("job-executor-details".into());
        binding.worker_session_id = Some(WorkerSessionId("worker-executor-details".into()));
        binding.codex_thread_id = Some(CodexThreadId("thread-executor-details".into()));
        binding.bound_at_millis = 1_800_000_000_011;
        Delivery::try_from_snapshot(snapshot).expect("writer Delivery")
    }

    #[test]
    fn live_diff_projection_exposes_summary_only() {
        let raw = "diff --git a/src/secret.rs b/src/secret.rs\n--- a/src/secret.rs\n+++ b/src/secret.rs\n@@ -1 +1,2 @@\n-old secret\n+new safe line\n+second line\n";
        let summary = summarize_live_diff(raw, "runtime:diff-1").expect("safe summary");
        assert_eq!(summary.changed_file_count(), 1);
        assert_eq!(summary.additions(), 2);
        assert_eq!(summary.deletions(), 1);
        assert!(!summary.details_visible());

        let value = serde_json::to_value(&summary).expect("summary json");
        assert_eq!(
            value,
            serde_json::json!({
                "changedFileCount": 1,
                "additions": 2,
                "deletions": 1,
                "detailsVisible": false,
                "sourceRef": "runtime:diff-1"
            })
        );
        let encoded = value.to_string();
        for forbidden in [
            "secret.rs",
            "old secret",
            "new safe line",
            "@@",
            "unifiedDiff",
        ] {
            assert!(!encoded.contains(forbidden));
        }
    }

    #[test]
    fn frozen_diff_details_require_current_finished_candidate() {
        let delivery = writer_delivery();
        let candidate = frozen_candidate(
            &delivery,
            &StageRunId("stage-executor-details".into()),
            &SessionBindingId("binding-executor-details".into()),
        );
        let details = test_support::accepted_frozen_candidate_details(
            &candidate,
            vec![CandidateHunkFact {
                file_path: "src/invitation.rs".into(),
                hunk_sha256: "b".repeat(64),
                source_hunk_sha256: None,
            }],
            "candidate:diff-details",
        );
        let grant = test_support::candidate_review_grant(&candidate, "reviewer-1", true);
        let projected = project_frozen_candidate_details(&delivery, &candidate, &details, &grant)
            .expect("current finished authorized candidate");
        assert_eq!(projected.paths[0].path, "src/invitation.rs");
        assert_eq!(projected.hunks[0].hunk_sha256, "b".repeat(64));

        let denied = test_support::candidate_review_grant(&candidate, "reviewer-1", false);
        assert_eq!(
            project_frozen_candidate_details(&delivery, &candidate, &details, &denied)
                .expect_err("review permission is required")
                .code(),
            ProjectionRedactionErrorCode::Unauthorized
        );

        let mut changed_spec = delivery.clone().into_snapshot();
        changed_spec.spec.revision += 1;
        changed_spec.revision += 1;
        let changed_spec = Delivery::try_from_snapshot(changed_spec).expect("new spec revision");
        assert_eq!(
            project_frozen_candidate_details(&changed_spec, &candidate, &details, &grant)
                .expect_err("stale candidate must not expose details")
                .code(),
            ProjectionRedactionErrorCode::StaleCandidate
        );

        let mut later = delivery.into_snapshot();
        later.stage_runs.push(StageRun {
            schema_version: crate::domain::DELIVERY_SCHEMA_VERSION,
            id: StageRunId("stage-later-writer".into()),
            delivery_id: later.id.clone(),
            delivery_task_id: later.stage_runs[0].delivery_task_id.clone(),
            stage: DeliveryStage::Executing,
            actor_type: StageRunActorType::Human,
            role: "executor".into(),
            status: StageRunStatus::Running,
            attempt: 2,
            started_at_millis: 1_800_000_000_030,
            finished_at_millis: None,
        });
        later.updated_at_millis = 1_800_000_000_030;
        let later = Delivery::try_from_snapshot(later).expect("later active writer");
        assert_eq!(
            project_frozen_candidate_details(&later, &candidate, &details, &grant)
                .expect_err("later active writer invalidates frozen details")
                .code(),
            ProjectionRedactionErrorCode::StaleCandidate
        );
    }

    #[test]
    fn public_projection_excludes_logs_payloads_and_credentials() {
        let delivery = Delivery::try_from_snapshot(test_fixture()).expect("canonical Delivery");
        let binding_id = delivery.snapshot().session_bindings[0].id.clone();
        let binding = accepted_binding(
            &delivery,
            &binding_id,
            RuntimeAuthorityFixture::default(),
            Some(1),
        )
        .expect("accepted fixture binding");
        let activity = RuntimeActivityProjection {
            call_id: "call-safe".into(),
            activity_type: RuntimeActivityType::Command,
            command: Some("cargo check -p winwincode-delivery".into()),
            status: RuntimeActivityStatus::Completed,
            outcome: RuntimeActivityOutcome::Succeeded,
            exit_code: Some(0),
            source_ref: "runtime:call-safe".into(),
        };
        let event = accepted_event(
            &binding,
            1,
            "runtime-event-safe",
            RuntimeFactFixture::Activity(activity),
        )
        .expect("safe semantic event");
        let mut projection =
            RuntimeProjection::new(&delivery, vec![binding.clone()]).expect("runtime projection");
        projection.apply(&event).expect("safe event projection");
        let encoded = serde_json::to_string(projection.snapshot()).expect("public projection JSON");
        for forbidden in [
            "apiKey",
            "authorization",
            "credential",
            "providerRequest",
            "providerResponse",
            "rawRuntimeLog",
            "stderr",
            "stdout",
            "toolPayload",
            "TOP_SECRET",
        ] {
            assert!(!encoded.contains(forbidden));
        }

        let secret_bearing = RuntimeActivityProjection {
            call_id: "call-secret".into(),
            activity_type: RuntimeActivityType::Command,
            command: Some("curl --token TOP_SECRET https://example.invalid".into()),
            status: RuntimeActivityStatus::Completed,
            outcome: RuntimeActivityOutcome::Succeeded,
            exit_code: Some(0),
            source_ref: "runtime:call-secret".into(),
        };
        assert!(
            accepted_event(
                &binding,
                1,
                "runtime-event-secret",
                RuntimeFactFixture::Activity(secret_bearing),
            )
            .is_err()
        );
    }
}
