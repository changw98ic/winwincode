// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::json;
use sha2::{Digest, Sha256};
use winwincode_delivery::{
    application::{
        stage::{
            DeliveryTerminalOutcomeFacts, TerminalArtifactReference, TerminalOutcomeStatus,
            test_support::{
                active_lease_identity, delivery_terminal_outcome_facts, session_binding_authority,
                terminal_outcome_metadata, terminal_worker_outcome,
            },
        },
        verdict::test_support::{VerdictFixtureOutcome, verdict_fixture},
    },
    domain::{
        DELIVERY_SCHEMA_VERSION, Delivery, RepositoryKind, RepositoryRef, StageRun,
        candidate::{
            ProductionRuntimeEvent, ProductionRuntimeEventCategory, ProductionRuntimePayload,
            ProductionVerificationRuntime, freeze_delivery_candidate_from_source,
            resolve_production_verdict,
        },
    },
};
use winwincode_domain::{
    ArtifactId, DeliveryId, ExecutionAckSequence, ExecutionEventId, ExecutionMessageId, Instant,
    OrganizationId, ProductSessionId, ProjectId, RepositoryId, RequestId, Sha256Digest, UserId,
    WorkspaceId,
};
use winwincode_storage::{
    ArtifactAccess, ArtifactChunk, ArtifactMeteringAttribution, ArtifactOpen, ArtifactProvenance,
    ArtifactRetention, ArtifactStore, CandidateSourceManifest, FakeArtifactObjectStore,
    LocalGitSourceResolver, ReceiptScopeKey, ValidatedGitSourceArtifact,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
struct SettledSource {
    terminal: DeliveryTerminalOutcomeFacts,
    source: ValidatedGitSourceArtifact,
}

#[test]
#[allow(clippy::too_many_lines)]
fn durable_sources_resolve_one_replay_stable_production_verdict() {
    let root = temporary_directory("success");
    let repositories = root.join("repositories");
    let repository = repositories.join("project-one");
    let (base_commit, candidate_commit) = repository_fixture(&repository);
    let fixture = verdict_fixture(
        &DeliveryId("dlv_01J00000000000000000000091".into()),
        VerdictFixtureOutcome::Pass,
    );
    let mut snapshot = fixture.delivery.into_snapshot();
    snapshot.spec.repository = RepositoryRef {
        schema_version: DELIVERY_SCHEMA_VERSION,
        kind: RepositoryKind::LocalGit,
        locator: "project-one".into(),
    };
    snapshot.spec.base_revision.clone_from(&base_commit);
    for (index, binding) in snapshot.session_bindings.iter_mut().enumerate() {
        let seed = 200 + index as u64;
        binding.execution_job_id = winwincode_domain::ExecutionJobId(canonical_id("job", seed));
        binding.worker_session_id = Some(winwincode_domain::WorkerSessionId(canonical_id(
            "wsn", seed,
        )));
        binding.codex_thread_id = Some(winwincode_domain::CodexThreadId(canonical_id("cdx", seed)));
        binding.lease_id = Some(winwincode_domain::LeaseId(canonical_id("lse", seed)));
        binding.fencing_token = Some(winwincode_domain::FencingToken(seed.to_string()));
        binding.worker_id = Some(winwincode_domain::WorkerId(canonical_id("wrk", seed)));
        binding.worker_instance_id = Some(winwincode_domain::WorkerInstanceId(canonical_id(
            "wki", seed,
        )));
        binding.attempt = 1;
    }
    let delivery = Delivery::try_from_snapshot(snapshot).expect("production Delivery");
    let scope = ReceiptScopeKey::from_encoded(b"repository:project-one".to_vec()).expect("scope");
    let mut artifacts = ArtifactStore::open(
        root.join("catalog"),
        Box::new(FakeArtifactObjectStore::new()),
    )
    .expect("Artifact store");
    let resolver = LocalGitSourceResolver::open(&repositories).expect("Git resolver");

    let writer = delivery
        .snapshot()
        .stage_runs
        .iter()
        .find(|run| run.role == "executor")
        .expect("writer");
    let writer_source = settled_source(
        &delivery,
        writer,
        &candidate_commit,
        &scope,
        &mut artifacts,
        &resolver,
        91,
    );
    let candidate = freeze_delivery_candidate_from_source(
        &delivery,
        &writer_source.source,
        &writer_source.terminal,
    )
    .expect("candidate");

    let verification_sources = ["reviewer", "verifier"]
        .into_iter()
        .enumerate()
        .map(|(index, role)| {
            let run = delivery
                .snapshot()
                .stage_runs
                .iter()
                .find(|run| run.role == role)
                .expect("verification run");
            let settled = settled_source(
                &delivery,
                run,
                &candidate_commit,
                &scope,
                &mut artifacts,
                &resolver,
                92 + index as u64,
            );
            (role, run, settled)
        })
        .collect::<Vec<_>>();
    let verification = verification_sources
        .iter()
        .map(|(role, run, settled)| {
            verification_runtime(
                &delivery,
                run,
                settled,
                candidate.candidate_ref(),
                role,
                None,
            )
        })
        .collect::<Vec<_>>();

    let first = resolve_production_verdict(
        &delivery,
        &writer_source.source,
        &writer_source.terminal,
        verification.clone(),
    )
    .expect("production verdict");
    let replay = resolve_production_verdict(
        &delivery,
        &writer_source.source,
        &writer_source.terminal,
        verification,
    )
    .expect("restart replay");
    assert_eq!(first, replay);
    let (candidate, verification, evidence, produced_at_millis) = first.into_parts();
    assert_eq!(candidate.candidate_commit_id(), candidate_commit);
    assert_eq!(verification.settlements().len(), 2);
    assert_eq!(evidence.len(), 2);
    assert!(
        evidence
            .iter()
            .all(|item| item.evidence().candidate_ref == candidate.candidate_ref())
    );
    assert!(produced_at_millis > 1_800_000_000_060);

    let stale_runtime = verification_sources
        .iter()
        .map(|(role, run, settled)| {
            verification_runtime(
                &delivery,
                run,
                settled,
                "git-candidate:stale-runtime",
                role,
                None,
            )
        })
        .collect::<Vec<_>>();
    resolve_production_verdict(
        &delivery,
        &writer_source.source,
        &writer_source.terminal,
        stale_runtime,
    )
    .expect_err("runtime bound to another candidate fails closed");

    let missing_evidence = verification_sources
        .iter()
        .map(|(role, run, settled)| {
            verification_runtime(
                &delivery,
                run,
                settled,
                candidate.candidate_ref(),
                role,
                Some(&format!("event-{role}-binary")),
            )
        })
        .collect::<Vec<_>>();
    resolve_production_verdict(
        &delivery,
        &writer_source.source,
        &writer_source.terminal,
        missing_evidence,
    )
    .expect_err("finding that cites non-JSON runtime Evidence fails closed");

    let mut stale_delivery = delivery.clone().into_snapshot();
    stale_delivery.spec.revision += 1;
    stale_delivery.revision += 1;
    stale_delivery.updated_at_millis += 1;
    let stale_delivery = Delivery::try_from_snapshot(stale_delivery).expect("stale Delivery");
    resolve_production_verdict(
        &stale_delivery,
        &writer_source.source,
        &writer_source.terminal,
        Vec::new(),
    )
    .expect_err("stale candidate/spec fails closed");

    artifacts.close().expect("Artifact close");
    fs::remove_dir_all(root).expect("fixture cleanup");
}

fn verification_runtime(
    delivery: &Delivery,
    run: &StageRun,
    settled: &SettledSource,
    candidate_ref: &str,
    role: &str,
    evidence_event_override: Option<&str>,
) -> ProductionVerificationRuntime {
    ProductionVerificationRuntime::from_durable(
        settled.terminal.clone(),
        settled.source.clone(),
        runtime_events(
            run,
            candidate_ref,
            &delivery.snapshot().spec.id.0,
            delivery.snapshot().spec.revision,
            &delivery.snapshot().spec.acceptance_criteria[0].id.0,
            role,
            evidence_event_override,
        ),
    )
}

#[allow(
    clippy::too_many_lines,
    reason = "the fixture writes one complete content-addressed Artifact and sealed terminal"
)]
fn settled_source(
    delivery: &Delivery,
    run: &StageRun,
    candidate_commit: &str,
    scope: &ReceiptScopeKey,
    artifacts: &mut ArtifactStore,
    resolver: &LocalGitSourceResolver,
    seed: u64,
) -> SettledSource {
    let binding = delivery
        .snapshot()
        .session_bindings
        .iter()
        .find(|binding| binding.stage_run_id == run.id)
        .expect("run binding");
    let worker_session = binding.worker_session_id.clone().expect("WorkerSession");
    let codex_thread = binding.codex_thread_id.clone().expect("CodexThread");
    let lease_id = binding
        .lease_id
        .clone()
        .unwrap_or_else(|| winwincode_domain::LeaseId(canonical_id("lse", seed)));
    let fencing_token = binding
        .fencing_token
        .clone()
        .unwrap_or_else(|| winwincode_domain::FencingToken(seed.to_string()));
    let worker_id = binding
        .worker_id
        .clone()
        .unwrap_or_else(|| winwincode_domain::WorkerId(canonical_id("wrk", seed)));
    let worker_instance = binding
        .worker_instance_id
        .clone()
        .unwrap_or_else(|| winwincode_domain::WorkerInstanceId(canonical_id("wki", seed)));
    let provenance = ArtifactProvenance::execution_job(
        binding.execution_job_id.clone(),
        run.attempt,
        lease_id.clone(),
        fencing_token.clone(),
        worker_id.clone(),
        worker_instance.clone(),
        worker_session.clone(),
    )
    .expect("Artifact provenance");
    let artifact_id = ArtifactId(canonical_id("art", seed));
    let manifest = CandidateSourceManifest::new(candidate_commit.to_owned())
        .expect("manifest")
        .encode()
        .expect("manifest encode");
    let digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(&manifest)));
    let finished_at = run.finished_at_millis.expect("finished run");
    artifacts
        .open_artifact(ArtifactOpen::new(
            scope.clone(),
            ExecutionMessageId(canonical_id("xmsg", seed * 2)),
            RequestId(canonical_id("req", seed * 2)),
            artifact_id.clone(),
            "candidate",
            "application/vnd.winwincode.git-candidate+json",
            digest.clone(),
            manifest.len() as u64,
            Some("candidate.json".into()),
            provenance.clone(),
            ArtifactMeteringAttribution {
                organization_id: OrganizationId(canonical_id("org", seed)),
                workspace_id: WorkspaceId(canonical_id("wsp", seed)),
                project_id: ProjectId(canonical_id("prj", seed)),
                repository_id: RepositoryId(canonical_id("rep", seed)),
                delivery_id: Some(delivery.id().clone()),
                product_session_id: Some(ProductSessionId(canonical_id("psn", seed))),
                user_id: UserId(canonical_id("usr", seed)),
            },
            ArtifactRetention::Indefinite,
            finished_at.saturating_sub(1),
        ))
        .expect("Artifact open");
    artifacts
        .append_chunk(&ArtifactChunk::new(
            scope.clone(),
            ExecutionMessageId(canonical_id("xmsg", seed * 2 + 1)),
            artifact_id.clone(),
            provenance.clone(),
            finished_at,
            1,
            "application/octet-stream",
            digest.clone(),
            manifest,
            true,
        ))
        .expect("Artifact complete");
    let object = artifacts
        .read_exact(&ArtifactAccess::new(
            scope.clone(),
            artifact_id.clone(),
            digest.clone(),
            provenance.clone(),
        ))
        .expect("Artifact read");
    let source = resolver
        .resolve_candidate(
            &object,
            &delivery.snapshot().spec.repository.locator,
            &delivery.snapshot().spec.base_revision,
        )
        .expect("source resolution");
    let authority = session_binding_authority(
        active_lease_identity(
            binding.execution_job_id.clone(),
            run.attempt,
            lease_id.clone(),
            fencing_token.clone(),
            worker_id.clone(),
            worker_instance.clone(),
            worker_session.clone(),
        ),
        Instant("2026-08-25T00:00:00.000Z".into()),
        Instant("2026-08-25T01:00:00.000Z".into()),
    );
    let terminal = delivery_terminal_outcome_facts(
        authority,
        terminal_worker_outcome(
            run.id.clone(),
            binding.execution_job_id.clone(),
            run.attempt,
            lease_id,
            fencing_token,
            worker_id,
            worker_instance,
            worker_session,
            TerminalOutcomeStatus::Succeeded,
            terminal_outcome_metadata(
                Some(codex_thread),
                finished_at,
                ExecutionAckSequence(4),
                vec![TerminalArtifactReference {
                    artifact_id,
                    digest,
                }],
            ),
        ),
    );
    SettledSource { terminal, source }
}

fn runtime_events(
    run: &StageRun,
    candidate_ref: &str,
    delivery_spec_id: &str,
    delivery_spec_revision: u64,
    criterion_id: &str,
    role: &str,
    evidence_event_override: Option<&str>,
) -> Vec<ProductionRuntimeEvent> {
    let finished = run.finished_at_millis.expect("finish");
    let source_id = ExecutionEventId(format!("event-{role}-source"));
    let category = if role == "reviewer" {
        ProductionRuntimeEventCategory::Command
    } else {
        ProductionRuntimeEventCategory::Test
    };
    let binary_bytes = b"\0verification-binary\xff";
    [
        (
            1,
            ProductionRuntimeEventCategory::Lifecycle,
            encoded_payload(
                &serde_json::to_vec(&json!({
                    "protocol": "winwincode.verification-session-policy.v1",
                    "workspace_mode": "candidate-read-only",
                    "permission_profile": "candidate-read-only-restricted",
                    "candidate_ref": candidate_ref,
                }))
                .expect("JSON"),
            ),
            ExecutionEventId(format!("event-{role}-policy")),
        ),
        (
            2,
            ProductionRuntimeEventCategory::Activity,
            ProductionRuntimePayload::from_validated_bytes(
                "application/octet-stream",
                binary_bytes.to_vec(),
            ),
            ExecutionEventId(format!("event-{role}-binary")),
        ),
        (
            3,
            category,
            encoded_payload(
                &serde_json::to_vec(&json!({"status": "completed", "exit_code": 0})).expect("JSON"),
            ),
            source_id.clone(),
        ),
        (
            4,
            ProductionRuntimeEventCategory::Activity,
            encoded_payload(
                &serde_json::to_vec(&json!({
                    "protocol": "winwincode.independent-verification-result.v1",
                    "delivery_spec_id": delivery_spec_id,
                    "delivery_spec_revision": delivery_spec_revision,
                    "candidate_ref": candidate_ref,
                    "findings": [{
                        "finding_id": format!("finding-{role}"),
                        "criterion_id": criterion_id,
                        "verdict": "pass",
                        "explanation": format!("{role} accepted the current candidate"),
                        "evidence_sources": [{
                            "type": if role == "reviewer" { "command" } else { "test" },
                            "event_id": evidence_event_override.unwrap_or(&source_id.0),
                        }],
                    }],
                }))
                .expect("JSON"),
            ),
            ExecutionEventId(format!("event-{role}-result")),
        ),
    ]
    .into_iter()
    .map(|(sequence, category, payload, event_id)| {
        ProductionRuntimeEvent::from_durable_ledger(
            category,
            event_id,
            sequence,
            finished.saturating_sub(5 - sequence),
            Some(payload),
        )
    })
    .collect()
}

fn encoded_payload(bytes: &[u8]) -> ProductionRuntimePayload {
    ProductionRuntimePayload::from_validated_bytes("application/json", bytes.to_vec())
}

fn repository_fixture(root: &Path) -> (String, String) {
    fs::create_dir_all(root).expect("repository root");
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["init", "-q", "-b", "main"])
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .expect("git init")
            .success()
    );
    fs::create_dir_all(root.join("src")).expect("source directory");
    fs::write(root.join("src/app.txt"), b"base\n").expect("base source");
    git(root, &["add", "--", "src/app.txt"]);
    commit(root, "base", "2026-08-25T00:00:00Z");
    let base = text(git(root, &["rev-parse", "HEAD"]));
    fs::write(root.join("src/app.txt"), b"base\ncandidate\n").expect("candidate source");
    git(root, &["add", "--", "src/app.txt"]);
    commit(root, "candidate", "2026-08-25T00:01:00Z");
    let candidate = text(git(root, &["rev-parse", "HEAD"]));
    (base, candidate)
}

fn git(root: &Path, arguments: &[&str]) -> Vec<u8> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(arguments)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("git command");
    assert!(
        output.status.success(),
        "git {arguments:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn commit(root: &Path, message: &str, timestamp: &str) {
    let status = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["commit", "-q", "-m", message])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "WinWinCode Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@winwincode.invalid")
        .env("GIT_COMMITTER_NAME", "WinWinCode Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@winwincode.invalid")
        .env("GIT_AUTHOR_DATE", timestamp)
        .env("GIT_COMMITTER_DATE", timestamp)
        .status()
        .expect("git commit");
    assert!(status.success());
}

fn text(value: Vec<u8>) -> String {
    String::from_utf8(value).expect("UTF-8").trim().to_owned()
}

fn canonical_id(prefix: &str, seed: u64) -> String {
    format!("{prefix}_{seed:026}")
}

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-production-verdict-{name}-{}-{suffix}",
        std::process::id()
    ))
}
