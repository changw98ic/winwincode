// SPDX-License-Identifier: Apache-2.0

#[test]
fn incremental_verification_commands_read_exact_policy_and_source_without_history_growth() {
    use crate::storage_mechanism_regression as measure;
    let mut samples = Vec::new();
    for size in [100_u64, 200, 400] {
        let root = std::env::temp_dir().join(format!(
            "wwc-indexed-stage-evidence-{}",
            uuid::Uuid::new_v4()
        ));
        let mut store = crate::store::AdapterStore::open(&root).unwrap();
        let fixture = fixture();
        let job = job("reviewer");
        let projector = WorkRunRuntimeProjector::new();
        ready(
            projector
                .retain_turn_started(
                    &mut store,
                    &AllowAuthority,
                    fixture.context(),
                    &job,
                    "turn-reviewer",
                )
                .unwrap(),
        );
        let command = vec!["git".to_owned(), "status".to_owned()];
        measure::reset();
        for sequence in 1..=size {
            let call = format!("call-{sequence:06}");
            ready(
                projector
                    .retain_exec_command_end(
                        &mut store,
                        &AllowAuthority,
                        fixture.context(),
                        &job,
                        StageCommandEnd {
                            command: &command,
                            turn_id: "turn-reviewer",
                            call_id: &call,
                            status: VerificationEvidenceStatus::Completed,
                            exit_code: 0,
                            artifact: None,
                        },
                    )
                    .unwrap(),
            );
        }
        let metrics = measure::finish();
        assert_eq!(metrics.replay_writes, 2 * size);
        assert!(metrics.replay_reads <= 8 * size + 2);
        let identity = replay_identity(fixture.context());
        let history = store.load(&identity.stream_key()).unwrap().unwrap();
        assert_eq!(history.events.len(), usize::try_from(size + 1).unwrap());
        assert!(snapshot_has_policy::<(), ()>(&history, &job).unwrap());
        samples.push(metrics.replay_read_bytes);
        eprintln!(
            "STORAGE_MECHANISM {}",
            serde_json::json!({"id":"verification-command-projection","size":size,"metrics":metrics,"retainedEvents":history.events.len()})
        );
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
    assert!(samples[1] < 3 * samples[0]);
    assert!(samples[2] < 3 * samples[1]);
}

fn prepare_indexed_policy_cache(
    store: &mut crate::store::AdapterStore,
    fixture: &Fixture,
    job: &ExecutionJob,
    projector: &WorkRunRuntimeProjector,
) -> (Box<RuntimeEventMessage>, Vec<String>) {
    let policy = ready(
        projector
            .retain_turn_started(
                store,
                &AllowAuthority,
                fixture.context(),
                job,
                "turn-reviewer",
            )
            .unwrap(),
    );
    let command = vec!["git".to_owned(), "status".to_owned()];
    ready(
        projector
            .retain_exec_command_end(
                store,
                &AllowAuthority,
                fixture.context(),
                job,
                StageCommandEnd {
                    command: &command,
                    turn_id: "turn-reviewer",
                    call_id: "first",
                    status: VerificationEvidenceStatus::Completed,
                    exit_code: 0,
                    artifact: None,
                },
            )
            .unwrap(),
    );
    (policy, command)
}

#[test]
fn indexed_verification_policy_cache_rechecks_immutable_body_and_stream_identity() {
    let root = std::env::temp_dir().join(format!(
        "wwc-indexed-policy-binding-{}",
        uuid::Uuid::new_v4()
    ));
    let mut store = crate::store::AdapterStore::open(&root).unwrap();
    let mut fixture = fixture();
    let job = job("reviewer");
    let projector = WorkRunRuntimeProjector::new();
    let (policy, command) = prepare_indexed_policy_cache(&mut store, &fixture, &job, &projector);
    let stream = replay_identity(fixture.context()).stream_key();
    let mut changed = (*policy).clone();
    changed.sent_at = Instant("2026-08-28T00:00:03.000Z".into());
    store
        .lock()
        .unwrap()
        .execute(
            "UPDATE runtime_replay_frame SET frame=?2 WHERE stream_key=?1 AND sequence=1",
            rusqlite::params![stream.as_str(), serde_json::to_vec(&changed).unwrap()],
        )
        .unwrap();
    assert!(
        projector
            .retain_exec_command_end(
                &mut store,
                &AllowAuthority,
                fixture.context(),
                &job,
                StageCommandEnd {
                    command: &command,
                    turn_id: "turn-reviewer",
                    call_id: "changed",
                    status: VerificationEvidenceStatus::Completed,
                    exit_code: 0,
                    artifact: None
                }
            )
            .is_err()
    );
    store
        .lock()
        .unwrap()
        .execute(
            "UPDATE runtime_replay_frame SET frame=?2 WHERE stream_key=?1 AND sequence=1",
            rusqlite::params![stream.as_str(), serde_json::to_vec(&*policy).unwrap()],
        )
        .unwrap();
    store
        .lock()
        .unwrap()
        .execute(
            "DELETE FROM runtime_replay_frame WHERE stream_key=?1 AND sequence=1",
            [stream.as_str()],
        )
        .unwrap();
    assert!(
        projector
            .retain_exec_command_end(
                &mut store,
                &AllowAuthority,
                fixture.context(),
                &job,
                StageCommandEnd {
                    command: &command,
                    turn_id: "turn-reviewer",
                    call_id: "missing",
                    status: VerificationEvidenceStatus::Completed,
                    exit_code: 0,
                    artifact: None
                }
            )
            .is_err()
    );
    fixture.lease.job_id.0 = "job_00000000000000000000000002".into();
    assert!(
        projector
            .retain_exec_command_end(
                &mut store,
                &AllowAuthority,
                fixture.context(),
                &job,
                StageCommandEnd {
                    command: &command,
                    turn_id: "turn-reviewer",
                    call_id: "foreign",
                    status: VerificationEvidenceStatus::Completed,
                    exit_code: 0,
                    artifact: None
                }
            )
            .is_err()
    );
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn indexed_verification_legacy_policy_hydrates_once_then_reads_exact_rows() {
    use crate::storage_mechanism_regression as measure;
    let root = std::env::temp_dir().join(format!(
        "wwc-indexed-legacy-policy-{}",
        uuid::Uuid::new_v4()
    ));
    let fixture = fixture();
    let job = job("reviewer");
    let mut store = crate::store::AdapterStore::open(&root).unwrap();
    let projector = WorkRunRuntimeProjector::new();
    let product = prepare_verification_policy_attestation(&job, CANDIDATE_REF).unwrap();
    let original = ready(Some(
        projector
            .retain_product(
                &mut store,
                &AllowAuthority,
                fixture.context(),
                &job,
                &source_key("policy", "before-upgrade", None),
                &product,
            )
            .unwrap(),
    ));
    let command = vec!["git".to_owned(), "status".to_owned()];
    for sequence in 1..=100 {
        let call = format!("legacy-call-{sequence:06}");
        ready(
            projector
                .retain_exec_command_end(
                    &mut store,
                    &AllowAuthority,
                    fixture.context(),
                    &job,
                    StageCommandEnd {
                        command: &command,
                        turn_id: "before-upgrade",
                        call_id: &call,
                        status: VerificationEvidenceStatus::Completed,
                        exit_code: 0,
                        artifact: None,
                    },
                )
                .unwrap(),
        );
    }
    drop(store);
    let mut store = crate::store::AdapterStore::open(&root).unwrap();
    let reopened = WorkRunRuntimeProjector::new();
    measure::reset();
    let retained = reopened
        .retain_turn_started(
            &mut store,
            &AllowAuthority,
            fixture.context(),
            &job,
            "after-upgrade",
        )
        .unwrap();
    assert!(matches!(
        retained,
        Some(WorkRunRuntimeRetention::Ready {
            duplicate: true,
            ..
        })
    ));
    assert_eq!(ready(retained).event, original.event);
    let first = measure::finish();
    assert!(
        first.replay_reads > 100,
        "the legacy identity is resolved by one full audit"
    );
    assert_eq!(first.replay_writes, 0);
    measure::reset();
    for sequence in 1..=100 {
        let turn = format!("repair-turn-{sequence:06}");
        let retained = reopened
            .retain_turn_started(&mut store, &AllowAuthority, fixture.context(), &job, &turn)
            .unwrap();
        assert!(matches!(
            retained,
            Some(WorkRunRuntimeRetention::Ready {
                duplicate: true,
                ..
            })
        ));
        assert_eq!(ready(retained).event, original.event);
    }
    let subsequent = measure::finish();
    assert!(subsequent.replay_reads <= 4 * 100);
    assert_eq!(subsequent.replay_writes, 0);
    let history = store
        .load(&replay_identity(fixture.context()).stream_key())
        .unwrap()
        .unwrap();
    assert_eq!(history.events.len(), 101);
    eprintln!(
        "STORAGE_MECHANISM {}",
        serde_json::json!({"id":"legacy-policy-projection","first":first,"subsequent":subsequent,"retainedEvents":history.events.len()})
    );
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn incremental_verification_turn_start_uses_constant_policy_reads_after_history_growth() {
    use crate::storage_mechanism_regression as measure;
    let mut per_turn_bytes = Vec::new();
    for size in [100_u64, 200, 400] {
        let root =
            std::env::temp_dir().join(format!("wwc-indexed-turn-policy-{}", uuid::Uuid::new_v4()));
        let mut store = crate::store::AdapterStore::open(&root).unwrap();
        let mut fixture = fixture();
        let job = job("reviewer");
        let projector = WorkRunRuntimeProjector::new();
        let original = ready(
            projector
                .retain_turn_started(
                    &mut store,
                    &AllowAuthority,
                    fixture.context(),
                    &job,
                    "first-turn",
                )
                .unwrap(),
        );
        let command = vec!["git".to_owned(), "status".to_owned()];
        for sequence in 1..=size {
            let call = format!("call-{sequence:06}");
            ready(
                projector
                    .retain_exec_command_end(
                        &mut store,
                        &AllowAuthority,
                        fixture.context(),
                        &job,
                        StageCommandEnd {
                            command: &command,
                            turn_id: "first-turn",
                            call_id: &call,
                            status: VerificationEvidenceStatus::Completed,
                            exit_code: 0,
                            artifact: None,
                        },
                    )
                    .unwrap(),
            );
        }
        // Renewal changes the accepted time period, while the owner, attempt,
        // fence and immutable policy source continue to address the same stream.
        fixture.lease.expires_at = Instant("2026-08-28T02:00:00.000Z".into());
        ready(
            projector
                .retain_turn_started(
                    &mut store,
                    &AllowAuthority,
                    fixture.context(),
                    &job,
                    "after-renewal",
                )
                .unwrap(),
        );
        measure::reset();
        for sequence in 1..=size {
            let turn = format!("repair-turn-{sequence:06}");
            let retained = projector
                .retain_turn_started(&mut store, &AllowAuthority, fixture.context(), &job, &turn)
                .unwrap();
            assert!(matches!(
                retained,
                Some(WorkRunRuntimeRetention::Ready {
                    duplicate: true,
                    ..
                })
            ));
            assert_eq!(ready(retained).event, original.event);
        }
        let metrics = measure::finish();
        assert!(metrics.replay_reads <= 4 * size);
        assert_eq!(metrics.replay_writes, 0);
        per_turn_bytes.push(metrics.replay_read_bytes / size);
        let history = store
            .load(&replay_identity(fixture.context()).stream_key())
            .unwrap()
            .unwrap();
        assert_eq!(history.events.len(), usize::try_from(size + 1).unwrap());
        eprintln!(
            "STORAGE_MECHANISM {}",
            serde_json::json!({"id":"verification-turn-policy","size":size,"metrics":metrics,"retainedEvents":history.events.len()})
        );
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
    assert_eq!(per_turn_bytes[0], per_turn_bytes[1]);
    assert_eq!(per_turn_bytes[1], per_turn_bytes[2]);
}

#[test]
fn indexed_verification_policy_rejects_second_attestation_before_and_after_cache_install() {
    for warm in [false, true] {
        let root = std::env::temp_dir().join(format!(
            "wwc-indexed-duplicate-policy-{}",
            uuid::Uuid::new_v4()
        ));
        let mut store = crate::store::AdapterStore::open(&root).unwrap();
        let fixture = fixture();
        let job = job("reviewer");
        let seeder = WorkRunRuntimeProjector::new();
        ready(
            seeder
                .retain_turn_started(
                    &mut store,
                    &AllowAuthority,
                    fixture.context(),
                    &job,
                    "first-turn",
                )
                .unwrap(),
        );
        let projector = WorkRunRuntimeProjector::new();
        if warm {
            ready(
                projector
                    .retain_turn_started(
                        &mut store,
                        &AllowAuthority,
                        fixture.context(),
                        &job,
                        "cache-turn",
                    )
                    .unwrap(),
            );
        }
        let product = prepare_verification_policy_attestation(&job, CANDIDATE_REF).unwrap();
        let event_id = projected_event_id(
            &job,
            &source_key("policy", "older-second-attestation", None),
        )
        .unwrap();
        let duplicate = build_message::<(), crate::store::AdapterStoreError>(
            fixture.context(),
            event_id,
            2,
            &product,
        )
        .unwrap();
        assert!(matches!(
            RuntimeReplayResponder::new()
                .retain_runtime_event(&mut store, &AllowAuthority, &duplicate)
                .unwrap(),
            ReplayDecision::Accepted { .. }
        ));
        assert!(matches!(
            projector.retain_turn_started(
                &mut store,
                &AllowAuthority,
                fixture.context(),
                &job,
                "repair-turn"
            ),
            Err(WorkRunRuntimeProjectionError::InvalidEvidence)
        ));
        let history = store
            .load(&replay_identity(fixture.context()).stream_key())
            .unwrap()
            .unwrap();
        assert_eq!(
            history.events.len(),
            2,
            "invalid history is retained for audit"
        );
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
}
