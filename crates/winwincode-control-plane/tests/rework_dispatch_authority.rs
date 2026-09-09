// SPDX-License-Identifier: Apache-2.0

use winwincode_control_plane::delivery_execution::{
    DeliveryExecutionConfig, prepare_workrun_advance,
};
use winwincode_delivery::{
    application::stage::{NewStageIdentities, StageAdvanceEffect, StageAdvanceResult},
    domain::{Delivery, SessionBindingId, rework::test_support::authorized_rework_dispatch},
};
use winwincode_domain::{AttentionItemId, ProductSessionId, RepositoryId, WorkItemId};
use winwincode_domain::{Instant, RequestId, Sha256Digest};
use winwincode_execution_port::generated::{ExecutionWorkspace, ExecutionWorkspaceWriteMode};

fn canonical_id(prefix: &str, n: u64) -> String {
    format!("{prefix}_{n:026}")
}

fn identities(
    intent: &winwincode_delivery::application::stage::ExecutionIntent,
) -> NewStageIdentities {
    NewStageIdentities {
        stage_run_id: intent.stage_run_id.clone(),
        work_contract_id: intent.work_contract_id.clone(),
        work_contract_revision: intent.work_contract_revision.clone(),
        work_item_id: intent.work_item_id.clone(),
        work_item_revision: intent.work_item_revision.clone(),
        work_run_id: intent.work_run_id.clone(),
        execution_job_id: intent.execution_job_id.clone(),
        session_binding_id: SessionBindingId(canonical_id("binding", 1)),
        attention_item_id: AttentionItemId(canonical_id("att", 1)),
    }
}

fn intent_and_auth(
    fixture: &winwincode_delivery::domain::rework::test_support::ReworkDispatchFixture,
) -> (
    &winwincode_delivery::application::stage::ExecutionIntent,
    Box<winwincode_delivery::domain::rework::ReworkAuthorization>,
) {
    let StageAdvanceEffect::Dispatch(intent) = &fixture.transition.effect else {
        panic!("fixture must contain a dispatch effect");
    };
    (
        intent,
        Box::new(
            intent
                .rework_authorization()
                .expect("fixture must contain authorization")
                .clone(),
        ),
    )
}

fn dispatch(
    source: &Delivery,
    intent: &winwincode_delivery::application::stage::ExecutionIntent,
    auth: Option<Box<winwincode_delivery::domain::rework::ReworkAuthorization>>,
    profile: &str,
) -> Result<StageAdvanceResult, winwincode_delivery::application::CoordinationError> {
    StageAdvanceResult::canonical_workrun_dispatch(
        source,
        auth,
        identities(intent),
        ProductSessionId(canonical_id("psn", 1)),
        profile.into(),
        intent.goal.clone(),
        intent.attempt,
        source.snapshot().updated_at_millis,
    )
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the authority test keeps every forged scope and replay rejection in one scenario"
)]
fn precise_rework_dispatch_authority_is_sealed_and_work_item_scoped() {
    let fixture =
        authorized_rework_dispatch(&winwincode_domain::DeliveryId(canonical_id("dlv", 1)));
    let (intent, auth) = intent_and_auth(&fixture);

    let accepted = dispatch(
        &fixture.source_delivery,
        intent,
        Some(auth.clone()),
        "remediator",
    )
    .expect("authorized rework dispatch");
    assert!(accepted.is_canonical_workrun_dispatch());
    assert!(accepted.delivery.snapshot().evidence.is_empty());
    assert!(accepted.delivery.snapshot().verdict.is_none());
    assert_eq!(
        accepted.delivery.snapshot().stage_runs,
        fixture.source_delivery.snapshot().stage_runs
    );
    let before = &fixture.source_delivery.snapshot().work_run_aggregate;
    let after = &accepted.delivery.snapshot().work_run_aggregate;
    let mut expected = before.clone();
    let producer = expected
        .runs
        .iter_mut()
        .find(|run| &run.id == auth.previous_candidate().producer_work_run_id())
        .unwrap();
    assert_eq!(
        producer.state,
        winwincode_domain::WorkRunState::CandidateReady
    );
    producer.state = winwincode_domain::WorkRunState::Settled;
    producer.revision.0 += 1;
    expected
        .items
        .iter_mut()
        .find(|item| &item.id == auth.work_item_id())
        .unwrap()
        .state = winwincode_domain::WorkItemState::Ready;
    assert_eq!(after, &expected);
    assert_eq!(intent.attempt, 1);
    assert!(before.start_item(auth.work_item_id()).is_err());
    let mut busy = fixture.source_delivery.clone().into_snapshot();
    let consumer_id = busy
        .session_bindings
        .iter()
        .find(|binding| binding.execution_profile.as_deref() == Some("reviewer"))
        .unwrap()
        .work_run_id
        .clone();
    let consumer = busy
        .work_run_aggregate
        .runs
        .iter_mut()
        .find(|run| run.id == consumer_id)
        .unwrap();
    consumer.work_item_id = auth.work_item_id().clone();
    consumer.state = winwincode_domain::WorkRunState::Running;
    // Test the canonical aggregate gate directly: an active reader blocks retirement.
    assert!(
        busy.work_run_aggregate
            .start_verification(auth.work_item_id())
            .is_err()
    );
    // The same seal cannot retire the candidate twice or mutate its input revision.
    assert!(auth.dispatch_aggregate(&accepted.delivery).is_err());
    assert_eq!(
        accepted.delivery.snapshot().session_bindings,
        fixture.source_delivery.snapshot().session_bindings
    );

    assert!(dispatch(&fixture.source_delivery, intent, None, "remediator").is_err());
    assert!(
        dispatch(
            &fixture.source_delivery,
            intent,
            Some(auth.clone()),
            "executor"
        )
        .is_err()
    );

    let mut changed = fixture.source_delivery.clone().into_snapshot();
    changed.revision += 1;
    changed.updated_at_millis += 1;
    let changed = Delivery::try_from_snapshot(changed).expect("changed source fixture");
    assert!(dispatch(&changed, intent, Some(auth.clone()), "remediator").is_err());

    for field in ["item", "item_revision", "contract_revision", "attempt"] {
        let mut wrong = intent.clone();
        match field {
            "item" => wrong.work_item_id = WorkItemId(canonical_id("wit", 999)),
            "item_revision" => wrong.work_item_revision.0 += 1,
            "contract_revision" => wrong.work_contract_revision.0 += 1,
            "attempt" => wrong.attempt += 1,
            _ => unreachable!(),
        }
        assert!(
            dispatch(
                &fixture.source_delivery,
                &wrong,
                Some(auth.clone()),
                "remediator"
            )
            .is_err(),
            "accepted {field}"
        );
    }
    let mut changed = fixture.source_delivery.clone().into_snapshot();
    for evidence in &mut changed.evidence {
        let original = evidence.id.clone();
        evidence.id.0.push_str("-replacement");
        for result in &mut changed.verdict.as_mut().unwrap().criteria {
            for id in &mut result.evidence_refs {
                if *id == original {
                    *id = evidence.id.clone();
                }
            }
        }
    }
    let changed = Delivery::try_from_snapshot(changed).expect("source with missing Evidence");
    assert!(dispatch(&changed, intent, Some(auth.clone()), "remediator").is_err());

    let config = DeliveryExecutionConfig {
        payload_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
        candidate_ref: Some(auth.candidate_ref().into()),
        workspace: ExecutionWorkspace {
            checkout_revision: auth.previous_candidate().candidate_commit_id().into(),
            repository_id: RepositoryId(canonical_id("rep", 1)),
            write_mode: ExecutionWorkspaceWriteMode::Candidate,
        },
        limits: winwincode_execution_port::generated::ExecutionLimits {
            deadline_at: Instant("2026-08-25T12:00:00.000Z".into()),
            max_artifact_bytes: 10_000,
            max_runtime_seconds: 60,
        },
    };
    let StageAdvanceEffect::Dispatch(accepted_intent) = &accepted.effect else {
        unreachable!()
    };
    let prepare = |config| {
        prepare_workrun_advance(
            &RequestId(canonical_id("req", 1)),
            &accepted.delivery.snapshot().work_run_aggregate,
            &accepted.delivery.snapshot().spec,
            accepted_intent,
            config,
        )
    };
    prepare(config.clone()).expect("correct IDs and source checkout must build the rework job");
    let mut wrong = config.clone();
    wrong.workspace.checkout_revision = "f".repeat(40);
    let error = prepare(wrong).expect_err("foreign source commit must be rejected");
    assert!(error.to_string().contains("rework authorization"));
    let mut wrong = config.clone();
    wrong.candidate_ref = Some(format!("git-candidate:sha256:{}", "f".repeat(64)));
    prepare(wrong).expect_err("work input must name the authorized source candidate");
    let mut wrong = config.clone();
    wrong.candidate_ref = None;
    prepare(wrong).expect_err("rework source candidate must be explicit in work input");
    prepare(config).expect("same job remains valid after rejected foreign checkout");
}

#[test]
fn rework_dispatch_selects_its_source_item_instead_of_an_unrelated_ready_item() {
    let fixture =
        authorized_rework_dispatch(&winwincode_domain::DeliveryId(canonical_id("dlv", 2)));
    let (intent, auth) = intent_and_auth(&fixture);
    let mut snapshot = fixture.source_delivery.clone().into_snapshot();
    let mut unrelated = snapshot.work_run_aggregate.items[0].clone();
    unrelated.id = WorkItemId(canonical_id("wit", 999));
    unrelated.state = winwincode_domain::WorkItemState::Ready;
    snapshot
        .work_run_aggregate
        .items
        .insert(0, unrelated.clone());
    let source = Delivery::try_from_snapshot(snapshot).unwrap();
    assert_eq!(
        source
            .snapshot()
            .work_run_aggregate
            .start_next()
            .unwrap()
            .work_item
            .id,
        unrelated.id
    );
    let auth =
        winwincode_delivery::domain::rework::test_support::precise_rework_authorization_fixture(
            &source,
            auth.previous_candidate(),
        );
    let accepted = dispatch(&source, intent, Some(Box::new(auth)), "remediator").unwrap();
    let StageAdvanceEffect::Dispatch(selected) = accepted.effect else {
        unreachable!()
    };
    assert_eq!(selected.work_item_id, intent.work_item_id);
    assert_ne!(selected.work_item_id, unrelated.id);
    assert_eq!(
        accepted.delivery.snapshot().work_run_aggregate.items[0],
        unrelated
    );
}
