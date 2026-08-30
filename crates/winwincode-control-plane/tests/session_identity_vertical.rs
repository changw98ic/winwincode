mod session_binding_fixture {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    use sha2::{Digest, Sha256};
    use winwincode_api::generated::{
        Actor, CommandEnvelope, CommandName, RepositoryScope, Scope, UserActor,
    };
    use winwincode_audit::{AuditEvent, AuditExecutionSubjectKind, AuditScope};
    use winwincode_control_plane::delivery_execution::{
        DeliveryExecutionConfig, DeliveryExecutionPortError, ExecutionJobDispatcher,
        PendingDeliveryExecution, prepare_delivery_advance,
    };
    use winwincode_control_plane::{
        CommitError, ControlPlane, ControlPlaneConfig, EventPublishError, EventPublisher,
        OutboxEvent, StateChange, StorageErrorKind,
    };
    use winwincode_delivery::application::stage::{
        AdvanceStageInput, NewStageIdentities, advance,
        test_support::{active_lease_identity, session_binding_authority},
    };
    use winwincode_delivery::domain::{
        DELIVERY_SCHEMA_VERSION, Delivery, DeliveryStatus, DeliveryTask, DeliveryTaskStatus,
        SessionBindingId,
    };
    use winwincode_delivery::store::{
        AtomicPublication, CreateDelivery, DeliveryCommand, DeliveryCommandPort,
        DeliveryJournalPort, DeliveryStore, JournalBackendError, LoadedDeliveryJournal,
    };
    use winwincode_domain::{
        AttentionItemId, CodexThreadId, DeliveryId, ExecutionAckSequence, ExecutionEventId,
        ExecutionJobId, ExecutionMessageId, ExecutionSequence, FencingToken, Instant, LeaseId,
        OrganizationId, ProductSessionId, ProjectId, RepositoryId, RequestId, Revision,
        SchemaVersion, SessionBindingSourceIdentity, SessionBindingSourceIdentityKind,
        SessionIdentity, Sha256Digest, StageRunId, UserId, WorkerId, WorkerInstanceId,
        WorkerSessionId, WorkspaceId,
    };
    use winwincode_execution_port::generated::{
        ExecutionEventCategory, ExecutionEventRecord, ExecutionJob, ExecutionLeaseStamp,
        ExecutionLimits, ExecutionPortErrorCode, ExecutionScope, ExecutionWorkspace,
        ExecutionWorkspaceWriteMode, LeaseWriteStatus, ProductSessionExecutionScope,
        ProductSessionExecutionScopeKind, RuntimeEventMessage, RuntimeEventMessageKind,
        SessionBindingMessage, SessionBindingMessageKind,
    };
    use winwincode_storage::{
        AggregateJournalKey, AggregateJournalPublication, AggregateJournalRecord, NewOutboxEvent,
        ProductStateStorage, PublicEventScope, ReceiptActorKey, ReceiptIdentity, ReceiptScopeKey,
        SqliteStorage, StateCommit, receipt_scope_key,
    };

    static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

    fn temporary_directory(name: &str) -> PathBuf {
        let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "winwincode-runtime-event-{name}-{}-{suffix}",
            std::process::id()
        ))
    }

    fn canonical_id(prefix: &str, value: u64) -> String {
        format!("{prefix}_{value:026}")
    }

    fn product_session_catalog_stream_id(scope: &RepositoryScope) -> String {
        let scope_key = receipt_scope_key(&PublicEventScope::Repository {
            organization_id: scope.organization_id.clone(),
            workspace_id: scope.workspace_id.clone(),
            project_id: scope.project_id.clone(),
            repository_id: scope.repository_id.clone(),
        })
        .expect("repository receipt scope");
        format!(
            "product-sessions:{:x}",
            Sha256::digest(scope_key.as_bytes())
        )
    }

    fn delivery_before_advance(seed: u64) -> Delivery {
        let mut snapshot = Delivery::decode_json(include_bytes!(
            "../../winwincode-delivery/tests/fixtures/delivery-main.json"
        ))
        .expect("canonical fixture")
        .into_snapshot();
        let delivery_id = DeliveryId(canonical_id("dlv", seed));
        snapshot.id = delivery_id.clone();
        snapshot.spec.delivery_id = delivery_id.clone();
        snapshot.revision = 1;
        snapshot.status = DeliveryStatus::Executing;
        snapshot.tasks = vec![DeliveryTask {
            schema_version: DELIVERY_SCHEMA_VERSION,
            id: winwincode_domain::DeliveryTaskId(canonical_id("dtk", seed)),
            delivery_id,
            title: "Implement the approved task".into(),
            goal: "Implement the approved candidate change.".into(),
            acceptance_criterion_ids: vec![snapshot.spec.acceptance_criteria[0].id.clone()],
            blocked_by_task_ids: Vec::new(),
            owner: None,
            status: DeliveryTaskStatus::Pending,
        }];
        snapshot.stage_runs.clear();
        snapshot.session_bindings.clear();
        snapshot.attention_items.clear();
        snapshot.evidence.clear();
        snapshot.verdict = None;
        snapshot.updated_at_millis = snapshot.created_at_millis;
        Delivery::try_from_snapshot(snapshot).expect("Delivery before advance")
    }

    fn pending_execution(seed: u64) -> PendingDeliveryExecution {
        let delivery = delivery_before_advance(seed);
        let result = advance(
            &delivery,
            AdvanceStageInput {
                current_lease: None,
                rework_authorization: None,
                expected_revision: 1,
                product_session_id: ProductSessionId(canonical_id("psn", seed)),
                identities: NewStageIdentities {
                    stage_run_id: StageRunId(canonical_id("run", seed)),
                    execution_job_id: ExecutionJobId(canonical_id("job", seed)),
                    session_binding_id: SessionBindingId::new(format!("binding-{seed}"))
                        .expect("binding id"),
                    attention_item_id: AttentionItemId(canonical_id("att", seed)),
                },
                review: None,
                previous_outcome: None,
                now_millis: 1_800_000_000_100,
            },
        )
        .expect("stage advance");
        prepare_delivery_advance(
            RequestId(canonical_id("req", seed)),
            result,
            DeliveryExecutionConfig {
                payload_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
                candidate_ref: None,
                workspace: ExecutionWorkspace {
                    checkout_revision: "original-checkout".into(),
                    repository_id: RepositoryId(canonical_id("rep", seed)),
                    write_mode: ExecutionWorkspaceWriteMode::Candidate,
                },
                limits: ExecutionLimits {
                    deadline_at: Instant("2027-01-15T09:00:00.000Z".into()),
                    max_artifact_bytes: 10_000_000,
                    max_runtime_seconds: 3_600,
                },
            },
        )
        .expect("pending execution")
    }

    fn delivery_advance_command(seed: u64) -> CommandEnvelope {
        CommandEnvelope {
            actor: Actor::UserActor(UserActor {
                id: UserId(canonical_id("usr", seed)),
                kind: winwincode_api::generated::UserActorKind::User,
            }),
            command: CommandName::DeliveryAdvance,
            expected_revision: Revision(1),
            payload: serde_json::json!({"deliveryId": canonical_id("dlv", seed)}),
            request_id: RequestId(canonical_id("req", seed)),
            schema_version: SchemaVersion::WinwincodeV1,
            scope: Scope::RepositoryScope(RepositoryScope {
                kind: winwincode_api::generated::RepositoryScopeKind::Repository,
                organization_id: OrganizationId(canonical_id("org", seed)),
                workspace_id: WorkspaceId(canonical_id("wsp", seed)),
                project_id: ProjectId(canonical_id("prj", seed)),
                repository_id: RepositoryId(canonical_id("rep", seed)),
            }),
        }
    }

    fn lease_and_message(
        pending: &PendingDeliveryExecution,
        seed: u64,
    ) -> (
        winwincode_delivery::application::stage::SessionBindingAuthority,
        SessionBindingMessage,
    ) {
        let worker_session_id = WorkerSessionId(canonical_id("wsn", seed));
        let worker_id = WorkerId(canonical_id("wrk", seed));
        let worker_instance_id = WorkerInstanceId(canonical_id("wki", seed));
        let lease_id = LeaseId(canonical_id("lse", seed));
        let fencing_token = FencingToken(seed.to_string());
        let lease = active_lease_identity(
            pending.job().job_id.clone(),
            1,
            lease_id.clone(),
            fencing_token.clone(),
            worker_id.clone(),
            worker_instance_id.clone(),
            worker_session_id.clone(),
        );
        let (stage_run_id, product_session_id) = match &pending.job().scope {
            ExecutionScope::DeliveryStageExecutionScope(scope) => {
                (scope.stage_run_id.clone(), scope.product_session_id.clone())
            }
            ExecutionScope::ProductSessionExecutionScope(_) => {
                panic!("vertical fixture must use a Delivery-stage job")
            }
        };
        let session_identity = SessionIdentity {
            codex_thread_id: CodexThreadId(canonical_id("cdx", seed)),
            product_session_id: product_session_id.clone(),
            stage_run_id: Some(stage_run_id.clone()),
            worker_session_id: worker_session_id.clone(),
        };
        let message = SessionBindingMessage {
            attempt: 1,
            bound_at: Instant("2027-01-15T08:00:01.000Z".into()),
            codex_thread_id: session_identity.codex_thread_id.clone(),
            fencing_token: fencing_token.clone(),
            kind: SessionBindingMessageKind::SessionBinding,
            lease: ExecutionLeaseStamp {
                attempt: 1,
                expires_at: Instant("2027-01-15T08:05:00.000Z".into()),
                fencing_token: fencing_token.clone(),
                issued_at: Instant("2027-01-15T08:00:00.200Z".into()),
                job_id: pending.job().job_id.clone(),
                lease_id: lease_id.clone(),
                worker_id: worker_id.clone(),
                worker_instance_id: worker_instance_id.clone(),
            },
            lease_id: lease_id.clone(),
            message_id: ExecutionMessageId(canonical_id("xmsg", seed)),
            product_session_id,
            schema_version: SchemaVersion::WinwincodeV1,
            sent_at: Instant("2027-01-15T08:00:01.100Z".into()),
            session_identity,
            source_identity: SessionBindingSourceIdentity {
                kind: SessionBindingSourceIdentityKind::ExecutionWorker,
                lease_id,
                worker_id: worker_id.clone(),
                worker_instance_id,
                worker_session_id: worker_session_id.clone(),
            },
            stage_run_id: Some(stage_run_id),
            worker_id,
            worker_session_id,
        };
        let authority = session_binding_authority(
            lease,
            message.lease.issued_at.clone(),
            message.lease.expires_at.clone(),
        );
        (authority, message)
    }

    fn running_fixture(
        seed: u64,
        name: &str,
    ) -> (
        PathBuf,
        ControlPlane,
        PendingDeliveryExecution,
        winwincode_delivery::application::stage::SessionBindingAuthority,
        SessionBindingMessage,
    ) {
        let root = temporary_directory(name);
        let pending = pending_execution(seed);
        seed_delivery(&root, &delivery_before_advance(seed));
        let mut control_plane = ControlPlane::start_local(
            ControlPlaneConfig::local(&root),
            Box::new(RecordingPublisher),
        )
        .expect("Control Plane start");
        control_plane
            .commit_delivery_execution(
                &delivery_advance_command(seed),
                &pending,
                &mut RecordingDispatcher,
            )
            .expect("Delivery execution commit");
        let (authority, message) = lease_and_message(&pending, seed);
        (root, control_plane, pending, authority, message)
    }

    fn durable_binding_counts(root: &Path, delivery_id: &DeliveryId) -> (i64, i64, i64, i64) {
        let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
            .expect("inspection database");
        let counts = connection
            .query_row(
                "SELECT \
                     (SELECT revision FROM product_state WHERE stream_id = ?1), \
                     (SELECT COUNT(*) FROM aggregate_journal_records WHERE aggregate_type = 'delivery' AND aggregate_id = ?2), \
                     (SELECT COUNT(*) FROM command_receipts WHERE stream_id = ?1 AND revision > 2), \
                     (SELECT COUNT(*) FROM outbox o JOIN command_receipts r \
                        ON r.actor_key = o.receipt_actor_key AND r.scope_key = o.receipt_scope_key \
                       AND r.request_id = o.request_id WHERE r.stream_id = ?1 AND r.revision > 2)",
                rusqlite::params![format!("delivery:{}", delivery_id.0), delivery_id.0],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("binding durable counts");
        connection.close().expect("inspection close");
        counts
    }

    fn durable_runtime_counts(root: &Path) -> (i64, i64, i64, i64, i64) {
        let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
            .expect("runtime inspection database");
        let counts = connection
            .query_row(
                "SELECT \
                     (SELECT COUNT(*) FROM product_state), \
                     (SELECT COUNT(*) FROM aggregate_journal_records), \
                     (SELECT COUNT(*) FROM command_receipts), \
                     (SELECT COUNT(*) FROM outbox), \
                     (SELECT COUNT(*) FROM audit_outbox)",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .expect("runtime durable counts");
        connection.close().expect("runtime inspection close");
        counts
    }

    fn pending_runtime_notifications(root: &Path) -> Vec<String> {
        let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
            .expect("pending notification inspection database");
        let mut statement = connection
            .prepare(
                "SELECT topic FROM outbox \
                 WHERE published = 0 \
                   AND topic IN ('runtime.event.accepted.v1', 'runtime-projection.invalidated.v1') \
                 ORDER BY sequence",
            )
            .expect("pending runtime notification query");
        let topics = statement
            .query_map([], |row| row.get::<_, String>(0))
            .expect("pending runtime notification rows")
            .collect::<Result<Vec<_>, _>>()
            .expect("pending runtime notification decode");
        drop(statement);
        connection
            .close()
            .expect("pending notification inspection close");
        topics
    }

    fn pending_outbox_events(root: &Path) -> Vec<(String, String)> {
        let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
            .expect("pending outbox inspection database");
        let mut statement = connection
            .prepare(
                "SELECT event_id, topic FROM outbox \
                 WHERE published = 0 \
                 ORDER BY sequence",
            )
            .expect("pending outbox query");
        let events = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("pending outbox rows")
            .collect::<Result<Vec<_>, _>>()
            .expect("pending outbox decode");
        drop(statement);
        connection.close().expect("pending outbox inspection close");
        events
    }

    fn stored_audit_events(root: &Path) -> Vec<(AuditEvent, i64)> {
        let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
            .expect("audit inspection database");
        let mut statement = connection
            .prepare("SELECT payload, persisted FROM audit_outbox ORDER BY event_id")
            .expect("audit outbox query");
        let encoded = statement
            .query_map([], |row| {
                Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, i64>(1)?))
            })
            .expect("audit outbox rows")
            .collect::<Result<Vec<_>, _>>()
            .expect("audit outbox row decode");
        drop(statement);
        connection.close().expect("audit inspection close");
        encoded
            .into_iter()
            .map(|(payload, persisted)| {
                (
                    serde_json::from_slice(&payload).expect("canonical AuditEvent payload"),
                    persisted,
                )
            })
            .collect()
    }

    #[derive(Default)]
    struct CapturingJournal {
        publication: Mutex<Option<AtomicPublication>>,
    }

    impl DeliveryJournalPort for CapturingJournal {
        fn load(
            &self,
            _delivery_id: &DeliveryId,
        ) -> Result<Option<LoadedDeliveryJournal>, JournalBackendError> {
            Ok(None)
        }

        fn publish(&self, publication: AtomicPublication) -> Result<(), JournalBackendError> {
            *self.publication.lock().expect("publication lock") = Some(publication);
            Ok(())
        }
    }

    fn seed_delivery(root: &Path, delivery: &Delivery) {
        let capture = CapturingJournal::default();
        DeliveryStore::borrowed(&capture)
            .execute(DeliveryCommand::SeedForTest(CreateDelivery {
                request_id: RequestId("c".repeat(64)),
                request_digest: "b".repeat(64),
                snapshot: delivery.clone(),
            }))
            .expect("seed Delivery journal publication");
        let AtomicPublication::Create {
            delivery_id,
            manifest,
            first_record,
        } = capture
            .publication
            .into_inner()
            .expect("publication lock")
            .expect("seed publication")
        else {
            panic!("seed must create the Delivery journal");
        };
        let publication = AggregateJournalPublication::Create {
            key: AggregateJournalKey::new("delivery", delivery_id.0).expect("journal key"),
            manifest,
            first_record: AggregateJournalRecord::new(
                first_record.sequence,
                first_record.digest,
                first_record.bytes,
            ),
        };
        let mut storage = SqliteStorage::open(root).expect("seed storage");
        let receipt = storage
            .commit(
                &StateCommit::new(
                    ReceiptIdentity::new(
                        ReceiptActorKey::from_encoded(b"seed-actor".to_vec()).expect("seed actor"),
                        ReceiptScopeKey::from_encoded(b"seed-scope".to_vec()).expect("seed scope"),
                        RequestId("c".repeat(64)),
                    )
                    .expect("seed identity"),
                    Sha256Digest(format!("sha256:{}", "b".repeat(64))),
                    format!("delivery:{}", delivery.id().0),
                    0,
                    delivery.encode_json().expect("seed Delivery JSON"),
                    vec![NewOutboxEvent::internal(
                        format!("seed-event-{}", delivery.id().0),
                        "delivery.seeded",
                        b"seed".to_vec(),
                    )],
                )
                .with_journal_publication(publication),
            )
            .expect("seed transaction");
        storage
            .mark_published(&receipt.events[0].event_id)
            .expect("seed event acknowledgement");
        Box::new(storage).close().expect("seed storage close");
    }

    #[derive(Default)]
    struct RecordingPublisher;

    impl EventPublisher for RecordingPublisher {
        fn publish(&mut self, _event: &OutboxEvent) -> Result<(), EventPublishError> {
            Ok(())
        }
    }

    struct FailingPublisher;

    impl EventPublisher for FailingPublisher {
        fn publish(&mut self, _event: &OutboxEvent) -> Result<(), EventPublishError> {
            Err(EventPublishError::new(
                "injected runtime notification failure",
            ))
        }
    }

    #[derive(Clone)]
    struct CountingFailingPublisher {
        calls: Arc<AtomicU64>,
    }

    impl EventPublisher for CountingFailingPublisher {
        fn publish(&mut self, _event: &OutboxEvent) -> Result<(), EventPublishError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(EventPublishError::new(
                "injected unrelated outbox publication failure",
            ))
        }
    }

    #[derive(Clone)]
    struct CapturingPublisher {
        events: Arc<Mutex<Vec<OutboxEvent>>>,
    }

    impl EventPublisher for CapturingPublisher {
        fn publish(&mut self, event: &OutboxEvent) -> Result<(), EventPublishError> {
            self.events
                .lock()
                .expect("captured events lock")
                .push(event.clone());
            Ok(())
        }
    }

    #[derive(Default)]
    struct RecordingDispatcher;

    impl ExecutionJobDispatcher for RecordingDispatcher {
        fn dispatch(&mut self, _job: &ExecutionJob) -> Result<(), DeliveryExecutionPortError> {
            Ok(())
        }
    }

    fn runtime_message(
        pending: &PendingDeliveryExecution,
        binding: &SessionBindingMessage,
        seed: u64,
    ) -> RuntimeEventMessage {
        let (stage_run_id, product_session_id) = match &pending.job().scope {
            ExecutionScope::DeliveryStageExecutionScope(scope) => {
                (scope.stage_run_id.clone(), scope.product_session_id.clone())
            }
            ExecutionScope::ProductSessionExecutionScope(_) => {
                panic!("vertical fixture must use a Delivery-stage job")
            }
        };
        RuntimeEventMessage {
            codex_thread_id: binding.codex_thread_id.clone(),
            event: ExecutionEventRecord {
                category: ExecutionEventCategory::Lifecycle,
                event_id: ExecutionEventId(canonical_id("xevt", seed + 100)),
                occurred_at: Instant("2027-01-15T08:00:01.050Z".into()),
                payload: None,
                sequence: ExecutionSequence(1),
                summary: "worker session started".into(),
            },
            kind: RuntimeEventMessageKind::RuntimeEvent,
            lease: binding.lease.clone(),
            message_id: ExecutionMessageId(canonical_id("xmsg", seed + 100)),
            schema_version: SchemaVersion::WinwincodeV1,
            sent_at: Instant("2027-01-15T08:00:01.100Z".into()),
            session_identity: SessionIdentity {
                codex_thread_id: binding.codex_thread_id.clone(),
                product_session_id,
                stage_run_id: Some(stage_run_id),
                worker_session_id: binding.worker_session_id.clone(),
            },
            worker_session_id: binding.worker_session_id.clone(),
        }
    }

    fn repository_scope(seed: u64) -> RepositoryScope {
        match delivery_advance_command(seed).scope {
            Scope::RepositoryScope(scope) => scope,
            _ => panic!("vertical fixture must use repository scope"),
        }
    }

    #[test]
    fn generated_runtime_event_with_foreign_codex_thread_is_rejected_before_any_delivery_member_changes()
     {
        let seed = 101;
        let (root, mut control_plane, pending, authority, binding) =
            running_fixture(seed, "foreign-runtime-codex-thread");
        control_plane
            .commit_delivery_session_binding(&binding, &authority, &binding.sent_at)
            .expect("exact SessionBinding must be accepted before runtime ingress");
        let before = durable_binding_counts(&root, pending.delivery().id());

        let mut runtime = runtime_message(&pending, &binding, seed);
        let foreign_codex = CodexThreadId(canonical_id("cdx", seed + 1_000));
        runtime.codex_thread_id = foreign_codex.clone();
        runtime.session_identity.codex_thread_id = foreign_codex;

        let scope = repository_scope(seed);
        let ack = control_plane
            .accept_runtime_event(&scope, &runtime, &authority, &runtime.sent_at)
            .expect("semantic rejection must return a generated RuntimeAck");

        assert_eq!(ack.status, LeaseWriteStatus::RejectedConflict);
        assert_eq!(ack.ack_sequence, ExecutionAckSequence(0));
        assert!(ack.replay_from_sequence.is_none());
        let error = ack.error.expect("foreign CodexThread must carry an error");
        assert_eq!(error.code, ExecutionPortErrorCode::MessageConflict);
        assert!(!error.retryable);
        assert_eq!(
            durable_binding_counts(&root, pending.delivery().id()),
            before
        );
        control_plane.shutdown().expect("shutdown");
        fs::remove_dir_all(root).expect("database directory release");
    }

    #[test]
    fn exact_runtime_event_replay_is_duplicate_but_changed_body_is_message_conflict() {
        let seed = 102;
        let (root, mut control_plane, pending, authority, binding) =
            running_fixture(seed, "runtime-replay");
        control_plane
            .commit_delivery_session_binding(&binding, &authority, &binding.sent_at)
            .expect("exact SessionBinding must be accepted before runtime ingress");
        let scope = repository_scope(seed);
        let runtime = runtime_message(&pending, &binding, seed);
        let before = durable_binding_counts(&root, pending.delivery().id());

        let first = control_plane
            .accept_runtime_event(&scope, &runtime, &authority, &runtime.sent_at)
            .expect("first runtime event");
        assert_eq!(first.status, LeaseWriteStatus::Accepted);
        assert_eq!(first.ack_sequence, ExecutionAckSequence(1));
        assert!(first.error.is_none());

        let duplicate = control_plane
            .accept_runtime_event(&scope, &runtime, &authority, &runtime.sent_at)
            .expect("exact runtime event replay");
        assert_eq!(duplicate.status, LeaseWriteStatus::Duplicate);
        assert_eq!(duplicate.ack_sequence, ExecutionAckSequence(1));
        assert!(duplicate.error.is_none());

        let mut changed_body = runtime.clone();
        changed_body.event.summary = "worker session changed".into();
        let before_conflict = durable_runtime_counts(&root);
        let conflict = control_plane
            .accept_runtime_event(&scope, &changed_body, &authority, &changed_body.sent_at)
            .expect("changed runtime event body conflict acknowledgement");
        assert_eq!(conflict.status, LeaseWriteStatus::RejectedConflict);
        assert_eq!(conflict.ack_sequence, ExecutionAckSequence(1));
        let error = conflict
            .error
            .as_ref()
            .expect("changed runtime event body must carry an error");
        assert_eq!(error.code, ExecutionPortErrorCode::MessageConflict);
        assert!(!error.retryable);
        let repeated_conflict = control_plane
            .accept_runtime_event(&scope, &changed_body, &authority, &changed_body.sent_at)
            .expect("repeated changed runtime event body conflict acknowledgement");
        assert_eq!(repeated_conflict, conflict);
        assert_eq!(durable_runtime_counts(&root), before_conflict);
        assert_eq!(
            durable_binding_counts(&root, pending.delivery().id()),
            before
        );
        control_plane.shutdown().expect("shutdown");
        fs::remove_dir_all(root).expect("database directory release");
    }

    #[test]
    fn same_runtime_event_digest_with_a_new_envelope_id_is_duplicate() {
        let seed = 103;
        let (root, mut control_plane, pending, authority, binding) =
            running_fixture(seed, "runtime-replay-new-envelope");
        control_plane
            .commit_delivery_session_binding(&binding, &authority, &binding.sent_at)
            .expect("exact SessionBinding must be accepted before runtime ingress");
        let scope = repository_scope(seed);
        let runtime = runtime_message(&pending, &binding, seed);

        let first = control_plane
            .accept_runtime_event(&scope, &runtime, &authority, &runtime.sent_at)
            .expect("first runtime event");
        assert_eq!(first.status, LeaseWriteStatus::Accepted);

        let mut replay = runtime.clone();
        replay.message_id = ExecutionMessageId(canonical_id("xmsg", seed + 1_000));
        let duplicate = control_plane
            .accept_runtime_event(&scope, &replay, &authority, &replay.sent_at)
            .expect("same event digest with a new envelope must be duplicate");
        assert_eq!(duplicate.status, LeaseWriteStatus::Duplicate);
        assert_eq!(duplicate.ack_sequence, ExecutionAckSequence(1));
        assert!(duplicate.error.is_none());

        replay.event.summary = "changed body must remain a conflict".into();
        let conflict = control_plane
            .accept_runtime_event(&scope, &replay, &authority, &replay.sent_at)
            .expect("changed event body must be a conflict");
        assert_eq!(conflict.status, LeaseWriteStatus::RejectedConflict);
        assert_eq!(conflict.ack_sequence, ExecutionAckSequence(1));
        assert_eq!(
            conflict
                .error
                .as_ref()
                .expect("changed event body error")
                .code,
            ExecutionPortErrorCode::MessageConflict
        );

        control_plane.shutdown().expect("shutdown");
        fs::remove_dir_all(root).expect("database directory release");
    }

    #[test]
    fn exact_runtime_event_across_eight_sqlite_connections_is_one_accept_and_seven_duplicates() {
        let seed = 109;
        let (root, mut control_plane, pending, authority, binding) =
            running_fixture(seed, "runtime-eight-connections");
        control_plane
            .commit_delivery_session_binding(&binding, &authority, &binding.sent_at)
            .expect("exact SessionBinding must be accepted before runtime ingress");
        let scope = repository_scope(seed);
        let runtime = runtime_message(&pending, &binding, seed);
        control_plane.shutdown().expect("fixture shutdown");

        let statuses = std::thread::scope(|threads| {
            let mut handles = Vec::new();
            for _ in 0..8 {
                let root = root.clone();
                let scope = scope.clone();
                let runtime = runtime.clone();
                let authority = authority.clone();
                handles.push(threads.spawn(move || {
                    let mut control_plane = ControlPlane::start_local(
                        ControlPlaneConfig::local(&root),
                        Box::new(RecordingPublisher),
                    )
                    .map_err(|error| error.to_string())?;
                    let ack = control_plane
                        .accept_runtime_event(&scope, &runtime, &authority, &runtime.sent_at)
                        .map_err(|error| error.to_string())?;
                    control_plane
                        .shutdown()
                        .map_err(|error| error.to_string())?;
                    Ok::<_, String>(ack.status)
                }));
            }
            handles
                .into_iter()
                .map(|handle| handle.join().expect("runtime connection thread"))
                .collect::<Result<Vec<_>, _>>()
        })
        .expect("all SQLite connections must accept the exact runtime event");

        assert_eq!(
            statuses
                .iter()
                .filter(|status| **status == LeaseWriteStatus::Accepted)
                .count(),
            1
        );
        assert_eq!(
            statuses
                .iter()
                .filter(|status| **status == LeaseWriteStatus::Duplicate)
                .count(),
            7
        );
        assert_eq!(pending_runtime_notifications(&root), Vec::<String>::new());
        fs::remove_dir_all(root).expect("database directory release");
    }

    #[test]
    fn semantic_runtime_rejection_does_not_flush_an_unrelated_pending_outbox_event() {
        let seed = 112;
        let (root, mut control_plane, pending, authority, binding) =
            running_fixture(seed, "runtime-rejection-unrelated-pending");
        control_plane
            .commit_delivery_session_binding(&binding, &authority, &binding.sent_at)
            .expect("exact SessionBinding must be accepted before runtime ingress");
        control_plane.shutdown().expect("fixture shutdown");

        let calls = Arc::new(AtomicU64::new(0));
        let mut control_plane = ControlPlane::start_local(
            ControlPlaneConfig::local(&root),
            Box::new(CountingFailingPublisher {
                calls: Arc::clone(&calls),
            }),
        )
        .expect("Control Plane start with counting failing publisher");
        let mut unrelated_command = delivery_advance_command(seed);
        unrelated_command.command = CommandName::SessionCancel;
        unrelated_command.expected_revision = Revision(0);
        unrelated_command.request_id = RequestId(canonical_id("req", seed + 10_000));
        let pending_error = control_plane
            .commit(
                &unrelated_command,
                StateChange::new(
                    "unrelated-runtime-test",
                    b"unrelated state",
                    vec![NewOutboxEvent::internal(
                        "unrelated-runtime-test-event",
                        "unrelated.runtime.test.v1",
                        b"unrelated notification".to_vec(),
                    )],
                ),
            )
            .expect_err("unrelated publisher failure must leave one event pending");
        assert!(matches!(
            pending_error,
            CommitError::PublicationPending { .. }
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let pending_before_rejection = pending_outbox_events(&root);
        assert_eq!(
            pending_before_rejection,
            vec![(
                "unrelated-runtime-test-event".to_owned(),
                "unrelated.runtime.test.v1".to_owned(),
            )]
        );

        let scope = repository_scope(seed);
        let mut foreign_runtime = runtime_message(&pending, &binding, seed);
        let foreign_codex = CodexThreadId(canonical_id("cdx", seed + 10_000));
        foreign_runtime.codex_thread_id = foreign_codex.clone();
        foreign_runtime.session_identity.codex_thread_id = foreign_codex;
        let rejected = control_plane
            .accept_runtime_event(
                &scope,
                &foreign_runtime,
                &authority,
                &foreign_runtime.sent_at,
            )
            .expect("semantic runtime rejection must return its generated ack directly");
        assert_eq!(rejected.status, LeaseWriteStatus::RejectedConflict);
        assert_eq!(rejected.ack_sequence, ExecutionAckSequence(0));
        assert_eq!(
            rejected
                .error
                .expect("foreign runtime identity must include an error")
                .code,
            ExecutionPortErrorCode::MessageConflict
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(pending_outbox_events(&root), pending_before_rejection);

        let _shutdown_error = control_plane
            .shutdown()
            .expect_err("pending unrelated event still fails its publisher on shutdown");
        fs::remove_dir_all(root).expect("database directory release");
    }

    #[test]
    fn changed_runtime_body_race_returns_conflict_acks_without_storage_errors() {
        let seed = 113;
        let (root, mut control_plane, pending, authority, binding) =
            running_fixture(seed, "runtime-changed-body-race");
        control_plane
            .commit_delivery_session_binding(&binding, &authority, &binding.sent_at)
            .expect("exact SessionBinding must be accepted before runtime ingress");
        control_plane.shutdown().expect("fixture shutdown");

        let scope = repository_scope(seed);
        let runtime = runtime_message(&pending, &binding, seed);
        let mut changed_runtime = runtime.clone();
        changed_runtime.event.summary = "changed body racing with the original".into();
        let statuses = std::thread::scope(|threads| {
            let mut handles = Vec::new();
            for index in 0..8 {
                let root = root.clone();
                let scope = scope.clone();
                let authority = authority.clone();
                let is_changed = index % 2 == 1;
                let runtime = if is_changed {
                    changed_runtime.clone()
                } else {
                    runtime.clone()
                };
                handles.push(threads.spawn(move || {
                    let mut control_plane = ControlPlane::start_local(
                        ControlPlaneConfig::local(&root),
                        Box::new(RecordingPublisher),
                    )
                    .map_err(|error| error.to_string())?;
                    let ack = control_plane
                        .accept_runtime_event(&scope, &runtime, &authority, &runtime.sent_at)
                        .map_err(|error| error.to_string())?;
                    control_plane
                        .shutdown()
                        .map_err(|error| error.to_string())?;
                    Ok::<_, String>((is_changed, ack))
                }));
            }
            handles
                .into_iter()
                .map(|handle| {
                    handle
                        .join()
                        .expect("changed-body runtime connection thread")
                })
                .collect::<Result<Vec<_>, _>>()
        })
        .expect("changed-body race must return generated acks, never StorageError");

        assert_eq!(
            statuses
                .iter()
                .filter(|(_, ack)| ack.status == LeaseWriteStatus::Accepted)
                .count(),
            1
        );
        let accepted_body_kind = statuses
            .iter()
            .find_map(|(is_changed, ack)| {
                (ack.status == LeaseWriteStatus::Accepted).then_some(*is_changed)
            })
            .expect("one racing body must win the receipt");
        for (is_changed, ack) in statuses {
            if is_changed == accepted_body_kind {
                assert!(matches!(
                    ack.status,
                    LeaseWriteStatus::Accepted | LeaseWriteStatus::Duplicate
                ));
            } else {
                assert_eq!(ack.status, LeaseWriteStatus::RejectedConflict);
                assert_eq!(
                    ack.error
                        .expect("changed-body race conflict must include an error")
                        .code,
                    ExecutionPortErrorCode::MessageConflict
                );
            }
        }
        fs::remove_dir_all(root).expect("database directory release");
    }

    #[test]
    fn exact_runtime_receipt_replay_survives_corrupt_current_job_state_and_audit() {
        let seed = 110;
        let (root, mut control_plane, pending, authority, binding) =
            running_fixture(seed, "runtime-replay-corrupt-current-facts");
        control_plane
            .commit_delivery_session_binding(&binding, &authority, &binding.sent_at)
            .expect("exact SessionBinding must be accepted before runtime ingress");
        let scope = repository_scope(seed);
        let runtime = runtime_message(&pending, &binding, seed);
        let accepted = control_plane
            .accept_runtime_event(&scope, &runtime, &authority, &runtime.sent_at)
            .expect("first runtime event");
        assert_eq!(accepted.status, LeaseWriteStatus::Accepted);
        control_plane.shutdown().expect("fixture shutdown");

        let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
            .expect("corruption database");
        connection
            .execute(
                "UPDATE outbox SET payload = ?1 WHERE event_id = ?2",
                rusqlite::params![
                    b"corrupt execution job".to_vec(),
                    format!("execution-job:{}", pending.job().job_id.0),
                ],
            )
            .expect("corrupt durable ExecutionJob payload");
        connection
            .execute(
                "UPDATE product_state SET payload = ?1 WHERE stream_id = ?2",
                rusqlite::params![
                    b"corrupt current Delivery state".to_vec(),
                    format!("delivery:{}", pending.delivery().id().0),
                ],
            )
            .expect("corrupt current Delivery state payload");
        connection
            .execute(
                "UPDATE audit_outbox SET payload = ?1 WHERE persisted = 1",
                rusqlite::params![b"corrupt accepted runtime audit".to_vec()],
            )
            .expect("corrupt persisted runtime audit payload");
        connection.close().expect("corruption database close");
        let before_replay = durable_runtime_counts(&root);

        let mut restarted = ControlPlane::start_local(
            ControlPlaneConfig::local(&root),
            Box::new(RecordingPublisher),
        )
        .expect("Control Plane restart with corrupt current facts");
        let replay = restarted
            .accept_runtime_event(&scope, &runtime, &authority, &runtime.sent_at)
            .expect("exact receipt replay must not reread current facts");
        assert_eq!(replay.status, LeaseWriteStatus::Duplicate);
        assert_eq!(replay.ack_sequence, ExecutionAckSequence(1));
        assert_eq!(durable_runtime_counts(&root), before_replay);
        restarted.shutdown().expect("restarted shutdown");
        fs::remove_dir_all(root).expect("database directory release");
    }

    #[test]
    fn runtime_event_sequence_gap_returns_replay_cursor_without_a_delivery_write() {
        let seed = 103;
        let (root, mut control_plane, pending, authority, binding) =
            running_fixture(seed, "runtime-gap");
        control_plane
            .commit_delivery_session_binding(&binding, &authority, &binding.sent_at)
            .expect("exact SessionBinding must be accepted before runtime ingress");
        let scope = repository_scope(seed);
        let before = durable_binding_counts(&root, pending.delivery().id());

        let mut gap = runtime_message(&pending, &binding, seed);
        gap.event.sequence = ExecutionSequence(2);
        let gap_ack = control_plane
            .accept_runtime_event(&scope, &gap, &authority, &gap.sent_at)
            .expect("sequence gap acknowledgement");
        assert_eq!(gap_ack.status, LeaseWriteStatus::Gap);
        assert_eq!(gap_ack.ack_sequence, ExecutionAckSequence(0));
        assert_eq!(gap_ack.replay_from_sequence, Some(ExecutionSequence(1)));
        let error = gap_ack.error.expect("sequence gap must carry an error");
        assert_eq!(error.code, ExecutionPortErrorCode::SequenceGap);
        assert!(error.retryable);
        assert_eq!(
            durable_binding_counts(&root, pending.delivery().id()),
            before
        );

        let first = runtime_message(&pending, &binding, seed);
        let first_ack = control_plane
            .accept_runtime_event(&scope, &first, &authority, &first.sent_at)
            .expect("missing first runtime event");
        assert_eq!(first_ack.status, LeaseWriteStatus::Accepted);
        assert_eq!(first_ack.ack_sequence, ExecutionAckSequence(1));

        let mut second_gap = runtime_message(&pending, &binding, seed + 1);
        second_gap.event.sequence = ExecutionSequence(3);
        let second_gap_ack = control_plane
            .accept_runtime_event(&scope, &second_gap, &authority, &second_gap.sent_at)
            .expect("second sequence gap acknowledgement");
        assert_eq!(second_gap_ack.status, LeaseWriteStatus::Gap);
        assert_eq!(second_gap_ack.ack_sequence, ExecutionAckSequence(1));
        assert_eq!(
            second_gap_ack.replay_from_sequence,
            Some(ExecutionSequence(2))
        );
        assert_eq!(
            second_gap_ack
                .error
                .expect("second sequence gap must carry an error")
                .code,
            ExecutionPortErrorCode::SequenceGap
        );
        assert_eq!(
            durable_binding_counts(&root, pending.delivery().id()),
            before
        );

        control_plane.shutdown().expect("shutdown");
        fs::remove_dir_all(root).expect("database directory release");
    }

    fn seed_second_delivery(root: &Path, delivery: &Delivery, seed: u64) {
        let capture = CapturingJournal::default();
        DeliveryStore::borrowed(&capture)
            .execute(DeliveryCommand::SeedForTest(CreateDelivery {
                request_id: RequestId(canonical_id("req", seed + 10_000)),
                request_digest: "d".repeat(64),
                snapshot: delivery.clone(),
            }))
            .expect("seed second Delivery journal publication");
        let AtomicPublication::Create {
            delivery_id,
            manifest,
            first_record,
        } = capture
            .publication
            .into_inner()
            .expect("publication lock")
            .expect("second seed publication")
        else {
            panic!("second seed must create the Delivery journal");
        };
        let publication = AggregateJournalPublication::Create {
            key: AggregateJournalKey::new("delivery", delivery_id.0).expect("journal key"),
            manifest,
            first_record: AggregateJournalRecord::new(
                first_record.sequence,
                first_record.digest,
                first_record.bytes,
            ),
        };
        let mut storage = SqliteStorage::open(root).expect("second seed storage");
        let receipt = storage
            .commit(
                &StateCommit::new(
                    ReceiptIdentity::new(
                        ReceiptActorKey::from_encoded(format!("seed-actor-{seed}").into_bytes())
                            .expect("second seed actor"),
                        ReceiptScopeKey::from_encoded(format!("seed-scope-{seed}").into_bytes())
                            .expect("second seed scope"),
                        RequestId(canonical_id("req", seed + 10_000)),
                    )
                    .expect("second seed identity"),
                    Sha256Digest(format!("sha256:{}", "e".repeat(64))),
                    format!("delivery:{}", delivery.id().0),
                    0,
                    delivery.encode_json().expect("second seed Delivery JSON"),
                    vec![NewOutboxEvent::internal(
                        format!("seed-event-{}", delivery.id().0),
                        "delivery.seeded",
                        b"seed".to_vec(),
                    )],
                )
                .with_journal_publication(publication),
            )
            .expect("second seed transaction");
        storage
            .mark_published(&receipt.events[0].event_id)
            .expect("second seed event acknowledgement");
        Box::new(storage)
            .close()
            .expect("second seed storage close");
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn runtime_event_restart_replays_one_delivery_and_keeps_a_second_delivery_isolated() {
        let seed_a = 104;
        let seed_b = 105;
        let root = temporary_directory("runtime-restart-two-deliveries");
        let pending_a = pending_execution(seed_a);
        let pending_b = pending_execution(seed_b);
        seed_delivery(&root, &delivery_before_advance(seed_a));
        seed_second_delivery(&root, &delivery_before_advance(seed_b), seed_b);
        let mut control_plane = ControlPlane::start_local(
            ControlPlaneConfig::local(&root),
            Box::new(RecordingPublisher),
        )
        .expect("Control Plane start");
        control_plane
            .commit_delivery_execution(
                &delivery_advance_command(seed_a),
                &pending_a,
                &mut RecordingDispatcher,
            )
            .expect("first Delivery execution commit");
        control_plane
            .commit_delivery_execution(
                &delivery_advance_command(seed_b),
                &pending_b,
                &mut RecordingDispatcher,
            )
            .expect("second Delivery execution commit");
        let (authority_a, binding_a) = lease_and_message(&pending_a, seed_a);
        let (authority_b, binding_b) = lease_and_message(&pending_b, seed_b);
        control_plane
            .commit_delivery_session_binding(&binding_a, &authority_a, &binding_a.sent_at)
            .expect("first SessionBinding");
        control_plane
            .commit_delivery_session_binding(&binding_b, &authority_b, &binding_b.sent_at)
            .expect("second SessionBinding");
        let scope_a = repository_scope(seed_a);
        let scope_b = repository_scope(seed_b);
        let runtime_a = runtime_message(&pending_a, &binding_a, seed_a);
        let runtime_b = runtime_message(&pending_b, &binding_b, seed_b);
        let before_a = durable_binding_counts(&root, pending_a.delivery().id());
        let before_b = durable_binding_counts(&root, pending_b.delivery().id());

        let accepted_a = control_plane
            .accept_runtime_event(&scope_a, &runtime_a, &authority_a, &runtime_a.sent_at)
            .expect("first Delivery runtime event");
        assert_eq!(accepted_a.status, LeaseWriteStatus::Accepted);

        let (runtime_audit, persisted) = stored_audit_events(&root)
            .into_iter()
            .find(|(event, _)| {
                event.subject().execution_kind() == Some(AuditExecutionSubjectKind::Runtime)
            })
            .expect("accepted runtime event must have one canonical audit event");
        assert_eq!(persisted, 1);
        let identity = runtime_audit
            .subject()
            .execution()
            .expect("runtime execution identity");
        assert_eq!(
            identity.product_session_id(),
            &runtime_a.session_identity.product_session_id
        );
        assert_eq!(identity.worker_session_id(), &runtime_a.worker_session_id);
        assert_eq!(identity.codex_thread_id(), &runtime_a.codex_thread_id);
        assert_eq!(
            Some(identity.stage_run_id()),
            runtime_a.session_identity.stage_run_id.as_ref()
        );
        assert_eq!(identity.execution_job_id(), &runtime_a.lease.job_id);
        assert_eq!(identity.delivery_id(), pending_a.delivery().id());
        if let ExecutionScope::DeliveryStageExecutionScope(job_scope) = &pending_a.job().scope {
            assert_eq!(
                identity.delivery_task_id(),
                job_scope.delivery_task_id.as_ref()
            );
        }
        assert_eq!(identity.worker_id(), &runtime_a.lease.worker_id);
        assert_eq!(
            identity.worker_instance_id(),
            &runtime_a.lease.worker_instance_id
        );
        assert_eq!(identity.lease_id(), &runtime_a.lease.lease_id);
        assert_eq!(identity.attempt(), 1);
        assert_eq!(identity.fencing_token(), &runtime_a.lease.fencing_token);
        assert_eq!(identity.source_sequence(), Some(&ExecutionAckSequence(1)));
        assert!(identity.binding_source().is_none());
        let audit_access = AuditScope::repository(
            scope_a.organization_id.clone(),
            scope_a.workspace_id.clone(),
            scope_a.project_id.clone(),
            scope_a.repository_id.clone(),
        )
        .expect("canonical runtime audit scope")
        .into_access();
        let audit_before_restart = control_plane
            .read_audit(&audit_access, 0, 20, 2_000_000_000_000)
            .expect("runtime event is visible through canonical AuditStore");
        assert!(audit_before_restart.records().iter().any(|record| {
            record
                .event()
                .is_some_and(|event| event.event_id() == runtime_audit.event_id())
        }));

        control_plane.shutdown().expect("first shutdown");
        let mut restarted = ControlPlane::start_local(
            ControlPlaneConfig::local(&root),
            Box::new(RecordingPublisher),
        )
        .expect("Control Plane restart");
        let audit_after_restart = restarted
            .read_audit(&audit_access, 0, 20, 2_000_000_000_000)
            .expect("runtime audit remains readable after restart flush");
        assert!(audit_after_restart.records().iter().any(|record| {
            record
                .event()
                .is_some_and(|event| event.event_id() == runtime_audit.event_id())
        }));

        let replay_a = restarted
            .accept_runtime_event(&scope_a, &runtime_a, &authority_a, &runtime_a.sent_at)
            .expect("first Delivery runtime replay after restart");
        assert_eq!(replay_a.status, LeaseWriteStatus::Duplicate);
        assert_eq!(replay_a.ack_sequence, ExecutionAckSequence(1));
        assert_eq!(
            stored_audit_events(&root)
                .iter()
                .filter(|(event, _)| {
                    event.subject().execution_kind() == Some(AuditExecutionSubjectKind::Runtime)
                })
                .count(),
            1
        );

        let accepted_b = restarted
            .accept_runtime_event(&scope_b, &runtime_b, &authority_b, &runtime_b.sent_at)
            .expect("second Delivery runtime event after restart");
        assert_eq!(accepted_b.status, LeaseWriteStatus::Accepted);
        assert_eq!(accepted_b.ack_sequence, ExecutionAckSequence(1));
        assert_eq!(
            stored_audit_events(&root)
                .iter()
                .filter(|(event, _)| {
                    event.subject().execution_kind() == Some(AuditExecutionSubjectKind::Runtime)
                })
                .count(),
            2
        );

        let replay_a_again = restarted
            .accept_runtime_event(&scope_a, &runtime_a, &authority_a, &runtime_a.sent_at)
            .expect("first Delivery replay remains isolated");
        assert_eq!(replay_a_again.status, LeaseWriteStatus::Duplicate);
        assert_eq!(
            durable_binding_counts(&root, pending_a.delivery().id()),
            before_a
        );
        assert_eq!(
            durable_binding_counts(&root, pending_b.delivery().id()),
            before_b
        );

        restarted.shutdown().expect("second shutdown");
        fs::remove_dir_all(root).expect("database directory release");
    }

    #[test]
    fn accepted_runtime_event_publishes_delivery_runtime_projection_invalidation() {
        let seed = 107;
        let root = temporary_directory("runtime-public-invalidation");
        let events = Arc::new(Mutex::new(Vec::new()));
        let pending = pending_execution(seed);
        seed_delivery(&root, &delivery_before_advance(seed));
        let mut control_plane = ControlPlane::start_local(
            ControlPlaneConfig::local(&root),
            Box::new(CapturingPublisher {
                events: Arc::clone(&events),
            }),
        )
        .expect("Control Plane start");
        control_plane
            .commit_delivery_execution(
                &delivery_advance_command(seed),
                &pending,
                &mut RecordingDispatcher,
            )
            .expect("Delivery execution commit");
        let (authority, binding) = lease_and_message(&pending, seed);
        control_plane
            .commit_delivery_session_binding(&binding, &authority, &binding.sent_at)
            .expect("SessionBinding");
        events.lock().expect("captured events lock").clear();

        let scope = repository_scope(seed);
        let runtime = runtime_message(&pending, &binding, seed);
        let ack = control_plane
            .accept_runtime_event(&scope, &runtime, &authority, &runtime.sent_at)
            .expect("runtime event");
        assert_eq!(ack.status, LeaseWriteStatus::Accepted);

        let captured = events.lock().expect("captured events lock");
        let invalidations = captured
            .iter()
            .filter(|event| event.topic == "runtime-projection.invalidated.v1")
            .collect::<Vec<_>>();
        assert_eq!(invalidations.len(), 1);
        assert!(matches!(
            invalidations[0].projection_cursor.as_ref().map(|cursor| cursor.key().stream()),
            Some(winwincode_control_plane::ProjectionEventStream::Delivery(id))
                if id == pending.delivery().id()
        ));

        control_plane.shutdown().expect("shutdown");
        fs::remove_dir_all(root).expect("database directory release");
    }

    #[test]
    fn runtime_publish_failure_retains_two_notifications_for_restart_replay() {
        let seed = 111;
        let (root, mut control_plane, pending, authority, binding) =
            running_fixture(seed, "runtime-publication-failure");
        control_plane
            .commit_delivery_session_binding(&binding, &authority, &binding.sent_at)
            .expect("exact SessionBinding must be accepted before runtime ingress");
        control_plane.shutdown().expect("fixture shutdown");

        let mut failing =
            ControlPlane::start_local(ControlPlaneConfig::local(&root), Box::new(FailingPublisher))
                .expect("Control Plane start with failing publisher");
        let scope = repository_scope(seed);
        let runtime = runtime_message(&pending, &binding, seed);
        let error = failing
            .accept_runtime_event(&scope, &runtime, &authority, &runtime.sent_at)
            .expect_err("publication failure must leave the accepted runtime event pending");
        let committed_ack = error
            .committed_ack()
            .expect("publication failure must retain the committed ack");
        assert_eq!(committed_ack.status, LeaseWriteStatus::Accepted);
        let _shutdown_error = failing
            .shutdown()
            .expect_err("failing publisher must report shutdown flush failure");
        assert_eq!(
            pending_runtime_notifications(&root),
            vec![
                "runtime.event.accepted.v1".to_owned(),
                "runtime-projection.invalidated.v1".to_owned(),
            ]
        );

        let events = Arc::new(Mutex::new(Vec::new()));
        let mut restarted = ControlPlane::start_local(
            ControlPlaneConfig::local(&root),
            Box::new(CapturingPublisher {
                events: Arc::clone(&events),
            }),
        )
        .expect("Control Plane restart must replay both notifications");
        let replayed_topics = events
            .lock()
            .expect("captured replay events lock")
            .iter()
            .map(|event| event.topic.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            replayed_topics,
            vec![
                "runtime.event.accepted.v1".to_owned(),
                "runtime-projection.invalidated.v1".to_owned(),
            ]
        );
        assert_eq!(pending_runtime_notifications(&root), Vec::<String>::new());

        let duplicate = restarted
            .accept_runtime_event(&scope, &runtime, &authority, &runtime.sent_at)
            .expect("exact runtime receipt replay after publication restart");
        assert_eq!(duplicate.status, LeaseWriteStatus::Duplicate);
        restarted.shutdown().expect("restarted shutdown");
        fs::remove_dir_all(root).expect("database directory release");
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn accepted_product_session_runtime_event_publishes_and_replays_product_invalidation() {
        let seed = 108;
        let root = temporary_directory("runtime-product-session-invalidation");
        let events = Arc::new(Mutex::new(Vec::new()));
        let scope = repository_scope(seed);
        let product_session_id = ProductSessionId(canonical_id("psn", seed));
        let job_id = ExecutionJobId(canonical_id("job", seed));
        let worker_session_id = WorkerSessionId(canonical_id("wsn", seed));
        let worker_id = WorkerId(canonical_id("wrk", seed));
        let worker_instance_id = WorkerInstanceId(canonical_id("wki", seed));
        let lease_id = LeaseId(canonical_id("lse", seed));
        let fencing_token = FencingToken(seed.to_string());
        let lease = ExecutionLeaseStamp {
            attempt: 1,
            expires_at: Instant("2027-01-15T08:05:00.000Z".into()),
            fencing_token: fencing_token.clone(),
            issued_at: Instant("2027-01-15T08:00:00.200Z".into()),
            job_id: job_id.clone(),
            lease_id: lease_id.clone(),
            worker_id: worker_id.clone(),
            worker_instance_id: worker_instance_id.clone(),
        };
        let authority = session_binding_authority(
            active_lease_identity(
                job_id.clone(),
                1,
                lease_id.clone(),
                fencing_token.clone(),
                worker_id.clone(),
                worker_instance_id.clone(),
                worker_session_id.clone(),
            ),
            lease.issued_at.clone(),
            lease.expires_at.clone(),
        );
        let job = ExecutionJob {
            attempt: 1,
            execution_profile: "codex".into(),
            goal: "ProductSession runtime projection".into(),
            job_id: job_id.clone(),
            limits: ExecutionLimits {
                deadline_at: Instant("2027-01-15T09:00:00.000Z".into()),
                max_artifact_bytes: 10_000_000,
                max_runtime_seconds: 3_600,
            },
            payload_digest: Sha256Digest(format!("sha256:{}", "f".repeat(64))),
            scope: ExecutionScope::ProductSessionExecutionScope(ProductSessionExecutionScope {
                kind: ProductSessionExecutionScopeKind::ProductSession,
                product_session_id: product_session_id.clone(),
            }),
            stage_input: None,
            workspace: ExecutionWorkspace {
                checkout_revision: "product-session-checkout".into(),
                repository_id: scope.repository_id.clone(),
                write_mode: ExecutionWorkspaceWriteMode::Candidate,
            },
        };
        let command = CommandEnvelope {
            actor: Actor::UserActor(UserActor {
                id: UserId(canonical_id("usr", seed)),
                kind: winwincode_api::generated::UserActorKind::User,
            }),
            command: CommandName::SessionCancel,
            expected_revision: Revision(0),
            payload: serde_json::json!({"productSessionId": product_session_id}),
            request_id: RequestId(canonical_id("req", seed)),
            schema_version: SchemaVersion::WinwincodeV1,
            scope: Scope::RepositoryScope(scope.clone()),
        };
        let mut control_plane = ControlPlane::start_local(
            ControlPlaneConfig::local(&root),
            Box::new(CapturingPublisher {
                events: Arc::clone(&events),
            }),
        )
        .expect("Control Plane start");
        control_plane
            .commit(
                &command,
                StateChange::new(
                    product_session_catalog_stream_id(&scope),
                    b"product-session-state".to_vec(),
                    vec![NewOutboxEvent::internal(
                        format!("execution-job:{}", job_id.0),
                        "execution.job.dispatch",
                        serde_json::to_vec(&job).expect("canonical ProductSession job JSON"),
                    )],
                ),
            )
            .expect("ProductSession ExecutionJob intent");
        events.lock().expect("captured events lock").clear();

        let codex_thread_id = CodexThreadId(canonical_id("cdx", seed));
        let runtime = RuntimeEventMessage {
            codex_thread_id: codex_thread_id.clone(),
            event: ExecutionEventRecord {
                category: ExecutionEventCategory::Lifecycle,
                event_id: ExecutionEventId(canonical_id("xevt", seed + 100)),
                occurred_at: Instant("2027-01-15T08:00:01.050Z".into()),
                payload: None,
                sequence: ExecutionSequence(1),
                summary: "product session worker started".into(),
            },
            kind: RuntimeEventMessageKind::RuntimeEvent,
            lease,
            message_id: ExecutionMessageId(canonical_id("xmsg", seed + 100)),
            schema_version: SchemaVersion::WinwincodeV1,
            sent_at: Instant("2027-01-15T08:00:01.100Z".into()),
            session_identity: SessionIdentity {
                codex_thread_id,
                product_session_id: product_session_id.clone(),
                stage_run_id: None,
                worker_session_id: worker_session_id.clone(),
            },
            worker_session_id,
        };
        let accepted = control_plane
            .accept_runtime_event(&scope, &runtime, &authority, &runtime.sent_at)
            .expect("ProductSession runtime event");
        assert_eq!(
            accepted.status,
            LeaseWriteStatus::Accepted,
            "{:?}",
            accepted.error
        );
        assert_eq!(accepted.ack_sequence, ExecutionAckSequence(1));
        assert!(accepted.session_identity.stage_run_id.is_none());

        let captured = events.lock().expect("captured events lock");
        let invalidations = captured
            .iter()
            .filter(|event| event.topic == "runtime-projection.invalidated.v1")
            .collect::<Vec<_>>();
        assert_eq!(invalidations.len(), 1);
        let payload: winwincode_api::generated::
            ControlPlaneWebSocketProductSessionRuntimeProjectionInvalidatedEvent =
            serde_json::from_slice(&invalidations[0].payload).expect("ProductSession DTO");
        assert_eq!(payload.product_session_id, product_session_id);
        assert_eq!(payload.projection_revision, Revision(1));
        assert_eq!(payload.last_projection_sequence, 1);
        assert_eq!(
            payload.scope_kind,
            winwincode_api::generated::
                ControlPlaneWebSocketProductSessionRuntimeProjectionInvalidatedEventScopeKind::ProductSession
        );
        assert!(matches!(
            invalidations[0]
                .projection_cursor
                .as_ref()
                .map(|cursor| cursor.key().stream()),
            Some(winwincode_control_plane::ProjectionEventStream::ProductSession(id))
                if id == &product_session_id
        ));
        drop(captured);

        control_plane.shutdown().expect("first shutdown");
        let mut restarted = ControlPlane::start_local(
            ControlPlaneConfig::local(&root),
            Box::new(RecordingPublisher),
        )
        .expect("Control Plane restart");
        let replay = restarted
            .accept_runtime_event(&scope, &runtime, &authority, &runtime.sent_at)
            .expect("ProductSession runtime replay");
        assert_eq!(replay.status, LeaseWriteStatus::Duplicate);
        assert_eq!(replay.ack_sequence, ExecutionAckSequence(1));
        assert!(replay.session_identity.stage_run_id.is_none());

        restarted.shutdown().expect("second shutdown");
        fs::remove_dir_all(root).expect("database directory release");
    }

    #[test]
    fn generic_control_plane_commit_cannot_bypass_the_typed_runtime_event_ledger() {
        let seed = 106;
        let (root, mut control_plane, pending, authority, binding) =
            running_fixture(seed, "runtime-generic-bypass");
        control_plane
            .commit_delivery_session_binding(&binding, &authority, &binding.sent_at)
            .expect("exact SessionBinding must be accepted before runtime ingress");
        let before = durable_binding_counts(&root, pending.delivery().id());
        let mut command = delivery_advance_command(seed);
        command.command = CommandName::SessionCancel;
        command.expected_revision = Revision(0);
        command.request_id = RequestId(canonical_id("req", seed + 1_000));

        let error = control_plane
            .commit(
                &command,
                StateChange::new(
                    "runtime:forged",
                    b"forged-runtime-ledger".to_vec(),
                    vec![NewOutboxEvent::internal(
                        "forged-runtime-event",
                        "runtime.event.accepted.v1",
                        b"forged".to_vec(),
                    )],
                ),
            )
            .expect_err("generic commit must not write the runtime ledger");
        assert!(matches!(
            error,
            CommitError::Storage(ref source) if source.kind() == StorageErrorKind::InvalidInput
        ));
        assert_eq!(
            durable_binding_counts(&root, pending.delivery().id()),
            before
        );

        control_plane.shutdown().expect("shutdown");
        fs::remove_dir_all(root).expect("database directory release");
    }
}
