// SPDX-License-Identifier: Apache-2.0

use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use winwincode_control_plane::peer_collaboration::{
    AdvanceCollaborationCommand, AgentAvailability, AgentDirectoryCommand, AgentSelector,
    AnswerClassification, CollaborationAdvanceAction, CollaborationContextRef,
    CollaborationPayload, CollaborationRequestId, CollaborationRequestState, CollaborationResult,
    PeerCollaborationClock, PeerCollaborationClockError, PeerCollaborationErrorKind,
    PeerCollaborationLane, PeerCollaborationService, PeerSessionPrincipal, ReviewFreshness,
    SubmitCollaborationCommand,
};
use winwincode_delivery::domain::{DeliverySpecId, EvidenceRef, EvidenceRefType, SessionBindingId};
use winwincode_domain::{
    DeliveryId, EvidenceId, ProductSessionId, RequestId, Sha256Digest, WorkItemId, WorkRunId,
    WorkerId, WorkerSessionId,
};
use winwincode_execution_port::{
    agent_config::AgentIdentity,
    generated::{WorkerCapabilityFeature, WorkerCapabilitySet, WorkerCapabilitySetPlatform},
};
use winwincode_storage::SqliteStorage;

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy)]
struct FixedClock(u64);

impl PeerCollaborationClock for FixedClock {
    fn now_millis(&mut self) -> Result<u64, PeerCollaborationClockError> {
        Ok(self.0)
    }
}

fn id(prefix: &str, seed: u64) -> String {
    format!("{prefix}_{seed:026}")
}

fn directory(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "winwincode-peer-collaboration-{name}-{}-{}",
        std::process::id(),
        NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
    ))
}

fn digest(character: char) -> Sha256Digest {
    Sha256Digest(format!("sha256:{}", character.to_string().repeat(64)))
}

fn agent(
    seed: u64,
    name: &str,
    role: &str,
    features: Vec<WorkerCapabilityFeature>,
) -> AgentIdentity {
    AgentIdentity {
        id: id("agt", seed),
        worker_id: WorkerId(id("wrk", seed)),
        name: name.to_owned(),
        role: role.to_owned(),
        capabilities: WorkerCapabilitySet {
            capability_digest: digest(char::from_digit((seed % 10) as u32, 10).unwrap()),
            features,
            max_concurrent_jobs: 2,
            platform: WorkerCapabilitySetPlatform::Aarch64AppleDarwin,
        },
    }
}

fn principal(identity: AgentIdentity, seed: u64) -> PeerSessionPrincipal {
    PeerSessionPrincipal {
        identity,
        session_id: WorkerSessionId(id("wsn", seed)),
    }
}

fn sync(
    service: &mut PeerCollaborationService<'_>,
    request_seed: u64,
    revision: u64,
    identity: AgentIdentity,
    session_id: Option<WorkerSessionId>,
    availability: AgentAvailability,
) {
    service
        .sync_agent(&AgentDirectoryCommand {
            request_id: RequestId(id("req", request_seed)),
            expected_catalog_revision: revision,
            identity,
            current_session_id: session_id,
            availability,
        })
        .expect("sync Agent Directory");
}

fn ask(question: &str) -> CollaborationPayload {
    CollaborationPayload::Ask {
        question: question.to_owned(),
        context_refs: vec![CollaborationContextRef::ProductSession {
            product_session_id: ProductSessionId(id("psn", 1)),
        }],
    }
}

fn submit(
    service: &mut PeerCollaborationService<'_>,
    principal: &PeerSessionPrincipal,
    seed: u64,
    revision: u64,
    target: &str,
    parent_request_id: Option<CollaborationRequestId>,
    payload: CollaborationPayload,
) -> winwincode_control_plane::peer_collaboration::PeerCollaborationReceipt {
    service
        .submit(
            principal,
            &SubmitCollaborationCommand {
                request_id: RequestId(id("req", seed)),
                expected_catalog_revision: revision,
                collaboration_request_id: CollaborationRequestId(id("col", seed)),
                target: AgentSelector::Identity {
                    name_or_role: target.to_owned(),
                },
                parent_request_id,
                payload,
            },
        )
        .expect("submit collaboration request")
}

fn evidence(candidate: char) -> EvidenceRef {
    EvidenceRef {
        schema_version: 1,
        id: EvidenceId(id("evd", 1)),
        delivery_id: DeliveryId(id("dlv", 1)),
        delivery_spec_id: DeliverySpecId("spec-v1".to_owned()),
        delivery_spec_revision: 1,
        work_run_id: WorkRunId(id("wrn", 1)),
        session_binding_id: SessionBindingId("binding:verifier".to_owned()),
        candidate_ref: format!("git-candidate:{}", digest(candidate).0),
        evidence_type: EvidenceRefType::Test,
        source_ref: "runtime-event:1".to_owned(),
        created_at_millis: 1_800_000_000_000,
    }
}

fn deliver_and_ack(
    service: &mut PeerCollaborationService<'_>,
    target: &PeerSessionPrincipal,
    command_seed: u64,
    revision: u64,
    request_id: &CollaborationRequestId,
) {
    for (offset, action) in [
        CollaborationAdvanceAction::Deliver,
        CollaborationAdvanceAction::Acknowledge,
    ]
    .into_iter()
    .enumerate()
    {
        service
            .advance(
                target,
                &AdvanceCollaborationCommand {
                    request_id: RequestId(id("req", command_seed + offset as u64)),
                    expected_catalog_revision: revision + offset as u64,
                    collaboration_request_id: request_id.clone(),
                    action,
                },
            )
            .expect("deliver and acknowledge request");
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn durable_request_survives_restart_and_returns_a_typed_answer() {
    let path = directory("restart");
    let source = principal(
        agent(
            1,
            "Executor",
            "executor",
            vec![WorkerCapabilityFeature::Shell],
        ),
        1,
    );
    let target_identity = agent(
        2,
        "Verifier",
        "verifier",
        vec![WorkerCapabilityFeature::Sandbox],
    );
    let request_id = CollaborationRequestId(id("col", 3));
    {
        let mut storage = SqliteStorage::open(&path).expect("open storage");
        let mut service = PeerCollaborationService::with_clock(
            &mut storage,
            Box::new(FixedClock(1_800_000_000_000)),
        );
        sync(
            &mut service,
            1,
            0,
            source.identity.clone(),
            Some(source.session_id.clone()),
            AgentAvailability::Working,
        );
        sync(
            &mut service,
            2,
            1,
            target_identity.clone(),
            None,
            AgentAvailability::Offline,
        );
        let receipt = submit(
            &mut service,
            &source,
            3,
            2,
            "Verifier",
            None,
            ask("Status?"),
        );
        assert_eq!(receipt.state, Some(CollaborationRequestState::Queued));
        let stored = service
            .get(&source, &request_id)
            .expect("read queued request");
        assert_eq!(stored.requester, source.identity);
        assert_eq!(stored.target_session_id, None);
    }

    let target = principal(target_identity, 2);
    let mut storage = SqliteStorage::open(&path).expect("reopen storage");
    let mut service =
        PeerCollaborationService::with_clock(&mut storage, Box::new(FixedClock(1_800_000_000_100)));
    sync(
        &mut service,
        4,
        3,
        target.identity.clone(),
        Some(target.session_id.clone()),
        AgentAvailability::Working,
    );
    deliver_and_ack(&mut service, &target, 5, 4, &request_id);
    let complete = AdvanceCollaborationCommand {
        request_id: RequestId(id("req", 7)),
        expected_catalog_revision: 6,
        collaboration_request_id: request_id.clone(),
        action: CollaborationAdvanceAction::Complete {
            result: CollaborationResult::AskAnswer {
                answer: "Verified by the current test evidence.".to_owned(),
                classification: AnswerClassification::AuthoritativeFact,
                evidence_refs: vec![evidence('a')],
            },
        },
    };
    service
        .advance(&target, &complete)
        .expect("complete request");
    let replay = service
        .advance(&target, &complete)
        .expect("replay completion");
    assert!(replay.idempotent_replay);
    let completed = service
        .get(&source, &request_id)
        .expect("read completed request");
    assert_eq!(completed.state, CollaborationRequestState::Completed);
    assert!(matches!(
        completed.result,
        Some(CollaborationResult::AskAnswer {
            classification: AnswerClassification::AuthoritativeFact,
            ..
        })
    ));
    drop(service);
    drop(storage);
    fs::remove_dir_all(path).expect("remove storage");
}

#[test]
#[allow(clippy::too_many_lines)]
fn directory_and_projection_cover_recovery_without_leaking_chat() {
    let path = directory("directory");
    let mut storage = SqliteStorage::open(&path).expect("open storage");
    let source = principal(
        agent(
            1,
            "Executor",
            "executor",
            vec![WorkerCapabilityFeature::Shell],
        ),
        1,
    );
    let target = principal(
        agent(
            2,
            "Verifier",
            "verifier",
            vec![WorkerCapabilityFeature::Sandbox],
        ),
        2,
    );
    let offline = agent(
        3,
        "Shell backup",
        "backup",
        vec![WorkerCapabilityFeature::Shell],
    );
    let mut service =
        PeerCollaborationService::with_clock(&mut storage, Box::new(FixedClock(1_800_000_000_000)));
    sync(
        &mut service,
        1,
        0,
        source.identity.clone(),
        Some(source.session_id.clone()),
        AgentAvailability::Working,
    );
    sync(
        &mut service,
        2,
        1,
        target.identity.clone(),
        Some(target.session_id.clone()),
        AgentAvailability::Recovering,
    );
    sync(
        &mut service,
        3,
        2,
        offline,
        None,
        AgentAvailability::Offline,
    );

    let by_identity = service
        .find_agents(&AgentSelector::Identity {
            name_or_role: "verifier".to_owned(),
        })
        .expect("find by identity");
    assert_eq!(
        by_identity.agents[0].availability,
        AgentAvailability::Recovering
    );
    let by_capability = service
        .find_agents(&AgentSelector::Capability {
            capability: WorkerCapabilityFeature::Shell,
        })
        .expect("find by capability");
    assert_eq!(by_capability.agents.len(), 2);
    assert_eq!(
        by_capability.agents[0].availability,
        AgentAvailability::Working
    );

    submit(
        &mut service,
        &source,
        4,
        3,
        "Verifier",
        None,
        CollaborationPayload::Consult {
            question: "SECRET CHAT BODY".to_owned(),
            context_refs: vec![CollaborationContextRef::ProductSession {
                product_session_id: ProductSessionId(id("psn", 1)),
            }],
        },
    );
    let rows = service.project(&source, &[]).expect("source projection");
    assert_eq!(rows[0].lane, PeerCollaborationLane::Waiting);
    assert!(
        !serde_json::to_string(&rows)
            .unwrap()
            .contains("SECRET CHAT BODY")
    );
    assert_eq!(
        service.project(&target, &[]).unwrap()[0].lane,
        PeerCollaborationLane::Inbox
    );

    let error = service
        .advance(
            &target,
            &AdvanceCollaborationCommand {
                request_id: RequestId(id("req", 5)),
                expected_catalog_revision: 4,
                collaboration_request_id: CollaborationRequestId(id("col", 4)),
                action: CollaborationAdvanceAction::Deliver,
            },
        )
        .expect_err("recovering Agent cannot advance work");
    assert_eq!(error.kind(), PeerCollaborationErrorKind::Unauthorized);
    sync(
        &mut service,
        6,
        4,
        target.identity.clone(),
        Some(target.session_id.clone()),
        AgentAvailability::Working,
    );
    deliver_and_ack(
        &mut service,
        &target,
        7,
        5,
        &CollaborationRequestId(id("col", 4)),
    );
    let unsupported_fact = service
        .advance(
            &target,
            &AdvanceCollaborationCommand {
                request_id: RequestId(id("req", 9)),
                expected_catalog_revision: 7,
                collaboration_request_id: CollaborationRequestId(id("col", 4)),
                action: CollaborationAdvanceAction::Complete {
                    result: CollaborationResult::ConsultAnswer {
                        answer: "Trust me.".to_owned(),
                        classification: AnswerClassification::AuthoritativeFact,
                        evidence_refs: Vec::new(),
                    },
                },
            },
        )
        .expect_err("facts require evidence");
    assert_eq!(
        unsupported_fact.kind(),
        PeerCollaborationErrorKind::InvalidRequest
    );
    service
        .advance(
            &target,
            &AdvanceCollaborationCommand {
                request_id: RequestId(id("req", 10)),
                expected_catalog_revision: 7,
                collaboration_request_id: CollaborationRequestId(id("col", 4)),
                action: CollaborationAdvanceAction::Complete {
                    result: CollaborationResult::ConsultAnswer {
                        answer: "My assessment.".to_owned(),
                        classification: AnswerClassification::Opinion,
                        evidence_refs: Vec::new(),
                    },
                },
            },
        )
        .expect("complete opinion");
    drop(service);
    drop(storage);
    fs::remove_dir_all(path).expect("remove storage");
}

#[test]
#[allow(clippy::too_many_lines)]
fn delegate_and_review_keep_independent_authority_and_candidate_binding() {
    let path = directory("delegate-review");
    let mut storage = SqliteStorage::open(&path).expect("open storage");
    let source = principal(agent(1, "Controller", "controller", vec![]), 1);
    let worker = principal(
        agent(2, "Worker", "worker", vec![WorkerCapabilityFeature::Shell]),
        2,
    );
    let verifier = principal(
        agent(
            3,
            "Verifier",
            "verifier",
            vec![WorkerCapabilityFeature::Sandbox],
        ),
        3,
    );
    let mut service =
        PeerCollaborationService::with_clock(&mut storage, Box::new(FixedClock(1_800_000_000_000)));
    for (seed, principal) in [(1, &source), (2, &worker), (3, &verifier)] {
        sync(
            &mut service,
            seed,
            seed - 1,
            principal.identity.clone(),
            Some(principal.session_id.clone()),
            AgentAvailability::Working,
        );
    }
    let parent_item = WorkItemId(id("wit", 1));
    let delegated_item = WorkItemId(id("wit", 2));
    submit(
        &mut service,
        &source,
        4,
        3,
        "Worker",
        None,
        CollaborationPayload::Delegate {
            parent_work_item_id: parent_item.clone(),
            delegated_work_item_id: delegated_item.clone(),
            objective: "Inspect one bounded module.".to_owned(),
            context_refs: vec![CollaborationContextRef::WorkItem {
                work_item_id: parent_item.clone(),
            }],
        },
    );
    let delegated = service
        .get(&source, &CollaborationRequestId(id("col", 4)))
        .expect("read delegation");
    assert_eq!(delegated.requester, source.identity);
    assert!(matches!(
        delegated.payload,
        CollaborationPayload::Delegate {
            parent_work_item_id,
            delegated_work_item_id,
            ..
        } if parent_work_item_id == parent_item && delegated_work_item_id == delegated_item
    ));
    let delegated_rows = service.project(&source, &[]).unwrap();
    assert_eq!(delegated_rows[0].lane, PeerCollaborationLane::Delegated);
    assert!(
        !serde_json::to_string(&delegated_rows)
            .unwrap()
            .contains("Inspect one bounded module")
    );
    let delegate_request_id = CollaborationRequestId(id("col", 4));
    deliver_and_ack(&mut service, &worker, 40, 4, &delegate_request_id);
    service
        .advance(
            &worker,
            &AdvanceCollaborationCommand {
                request_id: RequestId(id("req", 42)),
                expected_catalog_revision: 6,
                collaboration_request_id: delegate_request_id.clone(),
                action: CollaborationAdvanceAction::Complete {
                    result: CollaborationResult::DelegatedWork {
                        summary: "Bounded inspection complete.".to_owned(),
                        evidence_refs: vec![evidence('a')],
                    },
                },
            },
        )
        .expect("complete delegated work");
    let returned = service
        .get(&source, &delegate_request_id)
        .expect("result returns to original requester");
    assert_eq!(returned.requester, source.identity);
    assert!(matches!(
        returned.result,
        Some(CollaborationResult::DelegatedWork { .. })
    ));

    let candidate_a = digest('a');
    submit(
        &mut service,
        &source,
        5,
        7,
        "Verifier",
        None,
        CollaborationPayload::ReviewRequest {
            work_item_id: parent_item.clone(),
            candidate_digest: candidate_a.clone(),
            context_refs: vec![CollaborationContextRef::Candidate {
                work_item_id: parent_item.clone(),
                candidate_digest: candidate_a.clone(),
            }],
        },
    );
    let current = service
        .project(&source, &[(parent_item.clone(), candidate_a.clone())])
        .unwrap();
    assert_eq!(current[1].review_freshness, ReviewFreshness::Current);
    let stale = service
        .project(&source, &[(parent_item.clone(), digest('b'))])
        .unwrap();
    assert_eq!(stale[1].review_freshness, ReviewFreshness::Stale);

    deliver_and_ack(
        &mut service,
        &verifier,
        6,
        8,
        &CollaborationRequestId(id("col", 5)),
    );
    let mismatch = service
        .advance(
            &verifier,
            &AdvanceCollaborationCommand {
                request_id: RequestId(id("req", 8)),
                expected_catalog_revision: 10,
                collaboration_request_id: CollaborationRequestId(id("col", 5)),
                action: CollaborationAdvanceAction::Complete {
                    result: CollaborationResult::Review {
                        candidate_digest: digest('b'),
                        summary: "Reviewed another candidate.".to_owned(),
                        evidence_refs: vec![evidence('b')],
                    },
                },
            },
        )
        .expect_err("review cannot move to another candidate");
    assert_eq!(mismatch.kind(), PeerCollaborationErrorKind::InvalidState);
    drop(service);
    drop(storage);
    fs::remove_dir_all(path).expect("remove storage");
}

#[test]
#[allow(clippy::too_many_lines)]
fn duplicate_ping_pong_hop_and_rate_guards_fail_before_writes() {
    let path = directory("guards");
    let mut storage = SqliteStorage::open(&path).expect("open storage");
    let agents = (1..=7)
        .map(|seed| {
            principal(
                agent(
                    seed,
                    &format!("Agent{seed}"),
                    &format!("role{seed}"),
                    vec![],
                ),
                seed,
            )
        })
        .collect::<Vec<_>>();
    let mut service =
        PeerCollaborationService::with_clock(&mut storage, Box::new(FixedClock(1_800_000_000_000)));
    for (index, principal) in agents.iter().enumerate() {
        sync(
            &mut service,
            index as u64 + 1,
            index as u64,
            principal.identity.clone(),
            Some(principal.session_id.clone()),
            AgentAvailability::Working,
        );
    }
    submit(
        &mut service,
        &agents[0],
        100,
        7,
        "Agent2",
        None,
        ask("root"),
    );
    let duplicate = service
        .submit(
            &agents[0],
            &SubmitCollaborationCommand {
                request_id: RequestId(id("req", 101)),
                expected_catalog_revision: 8,
                collaboration_request_id: CollaborationRequestId(id("col", 101)),
                target: AgentSelector::Identity {
                    name_or_role: "Agent2".to_owned(),
                },
                parent_request_id: None,
                payload: ask("root"),
            },
        )
        .expect_err("duplicate active request");
    assert_eq!(duplicate.kind(), PeerCollaborationErrorKind::Duplicate);
    let ping_pong = service
        .submit(
            &agents[1],
            &SubmitCollaborationCommand {
                request_id: RequestId(id("req", 102)),
                expected_catalog_revision: 8,
                collaboration_request_id: CollaborationRequestId(id("col", 102)),
                target: AgentSelector::Identity {
                    name_or_role: "Agent1".to_owned(),
                },
                parent_request_id: Some(CollaborationRequestId(id("col", 100))),
                payload: ask("back"),
            },
        )
        .expect_err("ping-pong loop");
    assert_eq!(ping_pong.kind(), PeerCollaborationErrorKind::LoopDetected);

    let mut parent = CollaborationRequestId(id("col", 100));
    let mut revision = 8;
    for (index, principal) in agents.iter().enumerate().take(5).skip(1) {
        let seed = 110 + index as u64;
        submit(
            &mut service,
            principal,
            seed,
            revision,
            &format!("Agent{}", index + 2),
            Some(parent),
            ask(&format!("hop {index}")),
        );
        parent = CollaborationRequestId(id("col", seed));
        revision += 1;
    }
    let max_hop = service
        .submit(
            &agents[5],
            &SubmitCollaborationCommand {
                request_id: RequestId(id("req", 120)),
                expected_catalog_revision: revision,
                collaboration_request_id: CollaborationRequestId(id("col", 120)),
                target: AgentSelector::Identity {
                    name_or_role: "Agent7".to_owned(),
                },
                parent_request_id: Some(parent),
                payload: ask("one hop too far"),
            },
        )
        .expect_err("max hop");
    assert_eq!(max_hop.kind(), PeerCollaborationErrorKind::LoopDetected);

    for offset in 0..19 {
        let seed = 200 + offset;
        submit(
            &mut service,
            &agents[0],
            seed,
            revision,
            "Agent2",
            None,
            ask(&format!("rate request {offset}")),
        );
        revision += 1;
    }
    let limited = service
        .submit(
            &agents[0],
            &SubmitCollaborationCommand {
                request_id: RequestId(id("req", 300)),
                expected_catalog_revision: revision,
                collaboration_request_id: CollaborationRequestId(id("col", 300)),
                target: AgentSelector::Identity {
                    name_or_role: "Agent2".to_owned(),
                },
                parent_request_id: None,
                payload: ask("rate request 20"),
            },
        )
        .expect_err("rate limit");
    assert_eq!(limited.kind(), PeerCollaborationErrorKind::RateLimited);
    assert_eq!(service.catalog_revision().unwrap(), revision);
    drop(service);
    drop(storage);
    fs::remove_dir_all(path).expect("remove storage");
}
