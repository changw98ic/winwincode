// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::Connection;
use winwincode_api::generated::{
    Actor, ControlPlaneWebSocketAckFrame, ControlPlaneWebSocketAckFrameTypeValue,
    ControlPlaneWebSocketAcknowledgedCursor, ControlPlaneWebSocketActivityRecordedEvent,
    ControlPlaneWebSocketActivityRecordedEventTypeValue, ControlPlaneWebSocketClientFrame,
    ControlPlaneWebSocketControlPlaneSource, ControlPlaneWebSocketControlPlaneSourceKind,
    ControlPlaneWebSocketDeliveryChangedEvent, ControlPlaneWebSocketDeliveryChangedEventTypeValue,
    ControlPlaneWebSocketDeliveryTaskChangedEvent,
    ControlPlaneWebSocketDeliveryTaskChangedEventTypeValue, ControlPlaneWebSocketEventFrame,
    ControlPlaneWebSocketEventSource, ControlPlaneWebSocketEventType,
    ControlPlaneWebSocketResumeFrame, ControlPlaneWebSocketResumeFrameTypeValue,
    ControlPlaneWebSocketSubscribeFrame, ControlPlaneWebSocketSubscribeFrameTypeValue,
    ControlPlaneWebSocketSubscribeOrigin, ControlPlaneWebSocketSubscribeStartAt,
    ControlPlaneWebSocketSubscription, ControlPlaneWebSocketWorkerHealthChangedEvent,
    ControlPlaneWebSocketWorkerHealthChangedEventTypeValue, DeliveryEventReadStream,
    DeliveryEventReadStreamKind, EventReadStream, LeaseEventReadStream, LeaseEventReadStreamKind,
    Scope, ScopeEventReadStream, ScopeEventReadStreamKind,
};
use winwincode_domain::{
    ControlPlaneEventId, ControlPlaneWebSocketAuthorizationEpoch,
    ControlPlaneWebSocketSubscriptionId, DeliveryId, DeliveryTaskId, Instant, LeaseId,
    OrganizationId, ProjectId, RepositoryId, RequestId, Revision, Sha256Digest, UserId, WorkerId,
    WorkspaceId,
};
use winwincode_domain::{RepositoryScope, RepositoryScopeKind, UserActor, UserActorKind};
use winwincode_server::{
    AuthenticatedPrincipal, CommittedEventContext, DurableEventHub, DurableEventHubClock,
    DurableEventHubConfig,
};
use winwincode_storage::{
    NewOutboxEvent, ProductStateStorage, ProjectionEventStream, PublicEventActor, PublicEventScope,
    PublicEventSource, ReceiptIdentity, SqliteStorage, StateCommit, receipt_actor_key,
    receipt_scope_key,
};

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(1);
const FIXED_NOW_MILLIS: u64 = 1_787_792_400_000;

struct FixedClock;

impl DurableEventHubClock for FixedClock {
    fn now_millis(&self) -> u64 {
        FIXED_NOW_MILLIS
    }
}

struct Fixture {
    root: PathBuf,
    storage: SqliteStorage,
    hub: DurableEventHub,
    principal: AuthenticatedPrincipal,
    context: CommittedEventContext,
    public_scope: PublicEventScope,
    public_source: PublicEventSource,
    delivery_id: DeliveryId,
}

impl Fixture {
    fn new(config: DurableEventHubConfig) -> Self {
        let suffix = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "winwincode-durable-event-hub-{}-{suffix}",
            std::process::id()
        ));
        let scope = Scope::RepositoryScope(RepositoryScope {
            kind: RepositoryScopeKind::Repository,
            organization_id: OrganizationId("org_01J00000000000000000000000".to_owned()),
            workspace_id: WorkspaceId("wsp_01J00000000000000000000000".to_owned()),
            project_id: ProjectId("prj_01J00000000000000000000000".to_owned()),
            repository_id: RepositoryId("rep_01J00000000000000000000000".to_owned()),
        });
        let delivery_id = DeliveryId("dlv_01J00000000000000000000000".to_owned());
        let stream = EventReadStream::DeliveryEventReadStream(DeliveryEventReadStream {
            delivery_id: delivery_id.clone(),
            kind: DeliveryEventReadStreamKind::Delivery,
        });
        let source = ControlPlaneWebSocketEventSource::ControlPlaneWebSocketControlPlaneSource(
            ControlPlaneWebSocketControlPlaneSource {
                actor: Actor::UserActor(UserActor {
                    id: UserId("usr_01J00000000000000000000000".to_owned()),
                    kind: UserActorKind::User,
                }),
                component: "delivery-service".to_owned(),
                kind: ControlPlaneWebSocketControlPlaneSourceKind::ControlPlane,
            },
        );
        let public_scope = PublicEventScope::Repository {
            organization_id: OrganizationId("org_01J00000000000000000000000".to_owned()),
            workspace_id: WorkspaceId("wsp_01J00000000000000000000000".to_owned()),
            project_id: ProjectId("prj_01J00000000000000000000000".to_owned()),
            repository_id: RepositoryId("rep_01J00000000000000000000000".to_owned()),
        };
        let public_actor = PublicEventActor::User {
            id: UserId("usr_01J00000000000000000000000".to_owned()),
        };
        let public_source = PublicEventSource::ControlPlane {
            actor: public_actor,
            component: "delivery-service".to_owned(),
        };
        let storage = SqliteStorage::open(root.join("control-plane")).expect("open storage");
        let hub =
            DurableEventHub::open_with_clock(root.join("event-hub"), config, Arc::new(FixedClock))
                .expect("open event hub");
        let principal = AuthenticatedPrincipal::new(
            Actor::UserActor(UserActor {
                id: UserId("usr_01J00000000000000000000000".to_owned()),
                kind: UserActorKind::User,
            }),
            vec![scope.clone()],
        )
        .expect("principal");
        hub.grant_authorization(
            &principal,
            &scope,
            &ControlPlaneWebSocketAuthorizationEpoch(1),
        )
        .expect("grant authorization");
        Self {
            root,
            storage,
            hub,
            principal,
            context: CommittedEventContext {
                scope,
                stream,
                occurred_at: Instant("2026-08-27T01:00:00.000Z".to_owned()),
                source,
            },
            public_scope,
            public_source,
            delivery_id,
        }
    }

    fn publish(&mut self, sequence: u64) -> usize {
        let event = ControlPlaneWebSocketDeliveryChangedEvent {
            change_kind: "advanced".to_owned(),
            delivery_id: self.delivery_id.clone(),
            revision: Revision(i64::try_from(sequence).expect("fixture revision")),
            type_value: ControlPlaneWebSocketDeliveryChangedEventTypeValue::DeliveryChangedV1,
        };
        let payload = serde_json::to_vec(&event).expect("encode event");
        self.publish_payloads(vec![(sequence, payload)])
    }

    fn publish_task_range(&mut self, first: u64, last: u64) -> usize {
        let payloads = (first..=last)
            .map(|sequence| {
                let event = ControlPlaneWebSocketDeliveryTaskChangedEvent {
                    change_kind: "advanced".to_owned(),
                    delivery_id: self.delivery_id.clone(),
                    delivery_task_id: DeliveryTaskId(format!("dtk_{sequence:026}")),
                    revision: Revision(i64::try_from(sequence).expect("fixture revision")),
                    type_value:
                        ControlPlaneWebSocketDeliveryTaskChangedEventTypeValue::DeliveryTaskChangedV1,
                };
                (
                    sequence,
                    serde_json::to_vec(&event).expect("encode task event"),
                )
            })
            .collect();
        self.publish_payloads(payloads)
    }

    fn publish_payloads(&mut self, payloads: Vec<(u64, Vec<u8>)>) -> usize {
        self.commit_payloads(payloads);
        self.hub
            .publish_pending(&mut self.storage)
            .expect("publish committed event")
    }

    fn commit_payloads(&mut self, payloads: Vec<(u64, Vec<u8>)>) {
        let first = payloads.first().expect("non-empty fixture batch").0;
        let last = payloads.last().expect("non-empty fixture batch").0;
        let events = payloads
            .into_iter()
            .map(|(sequence, payload)| {
                NewOutboxEvent::public_projection(
                    ControlPlaneEventId(format!("evt_event_hub_{sequence:020}")),
                    "fixture.event.v1",
                    payload,
                    ProjectionEventStream::Delivery(self.delivery_id.clone()),
                    self.public_scope.clone(),
                    self.context.occurred_at.clone(),
                    self.public_source.clone(),
                )
                .expect("public outbox event")
            })
            .collect();
        self.storage
            .commit(&StateCommit::new(
                ReceiptIdentity::new(
                    receipt_actor_key(match &self.public_source {
                        PublicEventSource::ControlPlane { actor, .. } => actor,
                        _ => unreachable!("fixture source is Control Plane"),
                    })
                    .expect("actor key"),
                    receipt_scope_key(&self.public_scope).expect("scope key"),
                    RequestId(format!("req_event_hub_{first}_{last}")),
                )
                .expect("receipt"),
                Sha256Digest(format!("sha256:{last:064x}")),
                format!("event-hub-state-{first}-{last}"),
                0,
                format!("state-{first}-{last}").into_bytes(),
                events,
            ))
            .expect("commit event before publication");
    }

    fn subscription(
        &self,
        id: &str,
        start: ControlPlaneWebSocketSubscribeStartAt,
    ) -> ControlPlaneWebSocketClientFrame {
        self.subscription_for_types(
            id,
            start,
            vec![ControlPlaneWebSocketEventType::DeliveryChangedV1],
        )
    }

    fn subscription_for_types(
        &self,
        id: &str,
        start: ControlPlaneWebSocketSubscribeStartAt,
        event_types: Vec<ControlPlaneWebSocketEventType>,
    ) -> ControlPlaneWebSocketClientFrame {
        ControlPlaneWebSocketClientFrame::ControlPlaneWebSocketSubscribeFrame(
            ControlPlaneWebSocketSubscribeFrame {
                start_at: start,
                subscription: ControlPlaneWebSocketSubscription {
                    event_types,
                    scope: self.context.scope.clone(),
                    stream: self.context.stream.clone(),
                },
                subscription_id: ControlPlaneWebSocketSubscriptionId(id.to_owned()),
                type_value: ControlPlaneWebSocketSubscribeFrameTypeValue::TransportSubscribeV1,
            },
        )
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.hub.close();
        let replacement = SqliteStorage::open(self.root.join("replacement")).expect("replacement");
        let storage = std::mem::replace(&mut self.storage, replacement);
        let _ = Box::new(storage).close();
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn committed_publish_is_live_and_idempotent_without_credentials_in_frames() {
    let mut fixture = Fixture::new(DurableEventHubConfig::default());
    let mut subscription = fixture
        .hub
        .subscribe(
            &fixture.principal,
            fixture.subscription(
                "sub_event_hub_live",
                ControlPlaneWebSocketSubscribeStartAt::ControlPlaneWebSocketSubscribeOrigin(
                    ControlPlaneWebSocketSubscribeOrigin::Latest,
                ),
            ),
        )
        .expect("subscribe");
    assert_eq!(
        subscription.initial_frames[0]["type"],
        "transport.subscription-accepted.v1"
    );
    assert_eq!(fixture.publish(1), 1);
    let event = subscription.events.try_recv().expect("receive live event");
    assert_eq!(event["type"], "event.v1");
    assert_eq!(event["sequence"], 1);
    assert!(!event.to_string().contains("TOKEN_secret_fixture"));
    let duplicate = fixture
        .storage
        .load_outbox_event("evt_event_hub_00000000000000000001")
        .expect("load committed event")
        .expect("durable event");
    assert!(
        !fixture
            .hub
            .publish_committed(duplicate.event())
            .expect("idempotent direct republish")
    );
    assert!(subscription.events.try_recv().is_err());
    assert_eq!(
        fixture
            .hub
            .publish_pending(&mut fixture.storage)
            .expect("repeat publisher pass"),
        0
    );
    assert!(subscription.events.try_recv().is_err());
}

struct PublicStreamExpectation {
    stream: EventReadStream,
    event_type: ControlPlaneWebSocketEventType,
    event_id: &'static str,
}

fn commit_scope_and_lease_stream_events(fixture: &mut Fixture) -> [PublicStreamExpectation; 2] {
    let worker_id = WorkerId("wrk_01J00000000000000000000000".into());
    let lease_id = LeaseId("lse_01J00000000000000000000000".into());
    let scope_payload = serde_json::to_vec(&ControlPlaneWebSocketWorkerHealthChangedEvent {
        active_lease_count: 1,
        available_capacity: 3,
        capability_labels: Some(vec!["rust".into()]),
        observed_at: fixture.context.occurred_at.clone(),
        status: "healthy".into(),
        type_value: ControlPlaneWebSocketWorkerHealthChangedEventTypeValue::WorkerHealthChangedV1,
        worker_id: worker_id.clone(),
    })
    .expect("Scope event payload");
    let lease_payload = serde_json::to_vec(&ControlPlaneWebSocketActivityRecordedEvent {
        actor: Actor::UserActor(UserActor {
            id: UserId("usr_01J00000000000000000000000".into()),
            kind: UserActorKind::User,
        }),
        category: "lease".into(),
        delivery_id: None,
        product_session_id: None,
        summary: "Lease authority changed".into(),
        type_value: ControlPlaneWebSocketActivityRecordedEventTypeValue::ActivityRecordedV1,
    })
    .expect("Lease event payload");
    let events = vec![
        NewOutboxEvent::public_projection(
            ControlPlaneEventId("evt_scope_stream_round_trip_0001".into()),
            "worker-health.changed.v1",
            scope_payload,
            ProjectionEventStream::Scope,
            fixture.public_scope.clone(),
            fixture.context.occurred_at.clone(),
            fixture.public_source.clone(),
        )
        .expect("Scope public event"),
        NewOutboxEvent::public_projection(
            ControlPlaneEventId("evt_lease_stream_round_trip_0001".into()),
            "activity.recorded.v1",
            lease_payload,
            ProjectionEventStream::Lease {
                worker_id: worker_id.clone(),
                lease_id: lease_id.clone(),
            },
            fixture.public_scope.clone(),
            fixture.context.occurred_at.clone(),
            fixture.public_source.clone(),
        )
        .expect("Lease public event"),
    ];
    fixture
        .storage
        .commit(&StateCommit::new(
            ReceiptIdentity::new(
                receipt_actor_key(match &fixture.public_source {
                    PublicEventSource::ControlPlane { actor, .. } => actor,
                    _ => unreachable!("fixture source is Control Plane"),
                })
                .expect("actor key"),
                receipt_scope_key(&fixture.public_scope).expect("scope key"),
                RequestId("req_scope_lease_stream_round_trip".into()),
            )
            .expect("receipt"),
            Sha256Digest(format!("sha256:{}", "6".repeat(64))),
            "event-hub-scope-lease-round-trip",
            0,
            b"scope-lease".to_vec(),
            events,
        ))
        .expect("Scope and Lease events should commit");
    assert_eq!(
        fixture
            .hub
            .publish_pending(&mut fixture.storage)
            .expect("publish both stream kinds"),
        2
    );
    [
        PublicStreamExpectation {
            stream: EventReadStream::ScopeEventReadStream(ScopeEventReadStream {
                kind: ScopeEventReadStreamKind::Scope,
            }),
            event_type: ControlPlaneWebSocketEventType::WorkerHealthChangedV1,
            event_id: "evt_scope_stream_round_trip_0001",
        },
        PublicStreamExpectation {
            stream: EventReadStream::LeaseEventReadStream(LeaseEventReadStream {
                kind: LeaseEventReadStreamKind::Lease,
                worker_id,
                lease_id,
            }),
            event_type: ControlPlaneWebSocketEventType::ActivityRecordedV1,
            event_id: "evt_lease_stream_round_trip_0001",
        },
    ]
}

fn replay_exact_stream_frame(
    hub: &DurableEventHub,
    fixture: &Fixture,
    expectation: &PublicStreamExpectation,
    subscription_id: &str,
) -> ControlPlaneWebSocketEventFrame {
    let subscription = hub
        .subscribe(
            &fixture.principal,
            ControlPlaneWebSocketClientFrame::ControlPlaneWebSocketSubscribeFrame(
                ControlPlaneWebSocketSubscribeFrame {
                    start_at:
                        ControlPlaneWebSocketSubscribeStartAt::ControlPlaneWebSocketSubscribeOrigin(
                            ControlPlaneWebSocketSubscribeOrigin::EarliestAvailable,
                        ),
                    subscription: ControlPlaneWebSocketSubscription {
                        event_types: vec![expectation.event_type.clone()],
                        scope: fixture.context.scope.clone(),
                        stream: expectation.stream.clone(),
                    },
                    subscription_id: ControlPlaneWebSocketSubscriptionId(subscription_id.into()),
                    type_value: ControlPlaneWebSocketSubscribeFrameTypeValue::TransportSubscribeV1,
                },
            ),
        )
        .expect("subscribe to exact generated stream");
    serde_json::from_value(subscription.initial_frames[1].clone()).expect("generated event frame")
}

#[test]
fn scope_and_lease_streams_publish_as_exact_generated_streams_across_restart() {
    let mut fixture = Fixture::new(DurableEventHubConfig::default());
    let expectations = commit_scope_and_lease_stream_events(&mut fixture);
    let mut before_restart = expectations
        .iter()
        .enumerate()
        .map(|(index, expectation)| {
            replay_exact_stream_frame(
                &fixture.hub,
                &fixture,
                expectation,
                &format!("sub_new_stream_before_{index}"),
            )
        })
        .collect::<Vec<_>>();
    fixture.hub.close().expect("close before replay restart");
    let restarted = DurableEventHub::open(
        fixture.root.join("event-hub"),
        DurableEventHubConfig::default(),
    )
    .expect("restart event hub");
    for (index, (expectation, before)) in expectations
        .iter()
        .zip(before_restart.iter_mut())
        .enumerate()
    {
        let mut after = replay_exact_stream_frame(
            &restarted,
            &fixture,
            expectation,
            &format!("sub_new_stream_after_{index}"),
        );
        assert_eq!(before.stream, expectation.stream);
        assert_eq!(before.event_id.0, expectation.event_id);
        before.subscription_id = ControlPlaneWebSocketSubscriptionId("sub_normalized".into());
        after.subscription_id = ControlPlaneWebSocketSubscriptionId("sub_normalized".into());
        assert_eq!(
            serde_json::to_vec(&after).expect("restarted frame bytes"),
            serde_json::to_vec(before).expect("first frame bytes")
        );
    }
    restarted.close().expect("close restarted event hub");
}

#[test]
fn crash_replay_preserves_the_exact_public_event_frame() {
    let mut fixture = Fixture::new(DurableEventHubConfig::default());
    let first_payload = serde_json::to_vec(&ControlPlaneWebSocketDeliveryChangedEvent {
        change_kind: "advanced".to_owned(),
        delivery_id: fixture.delivery_id.clone(),
        revision: Revision(1),
        type_value: ControlPlaneWebSocketDeliveryChangedEventTypeValue::DeliveryChangedV1,
    })
    .expect("first payload");
    fixture.commit_payloads(vec![(1, first_payload)]);

    fixture.hub.close().expect("crash before first hub write");
    let first_restart = DurableEventHub::open(
        fixture.root.join("event-hub"),
        DurableEventHubConfig::default(),
    )
    .expect("first restart");
    assert_eq!(
        first_restart
            .publish_pending(&mut fixture.storage)
            .expect("publish after first restart"),
        1
    );

    let mut subscription = first_restart
        .subscribe(
            &fixture.principal,
            fixture.subscription(
                "sub_crash_exact_before",
                ControlPlaneWebSocketSubscribeStartAt::ControlPlaneWebSocketSubscribeOrigin(
                    ControlPlaneWebSocketSubscribeOrigin::EarliestAvailable,
                ),
            ),
        )
        .expect("subscribe after first restart");
    assert_eq!(
        subscription.initial_frames[1]["occurredAt"],
        fixture.context.occurred_at.0
    );
    assert_eq!(
        subscription.initial_frames[1]["source"]["component"],
        "delivery-service"
    );

    let second_payload = serde_json::to_vec(&ControlPlaneWebSocketDeliveryChangedEvent {
        change_kind: "advanced".to_owned(),
        delivery_id: fixture.delivery_id.clone(),
        revision: Revision(2),
        type_value: ControlPlaneWebSocketDeliveryChangedEventTypeValue::DeliveryChangedV1,
    })
    .expect("second payload");
    fixture.commit_payloads(vec![(2, second_payload)]);
    let event_id = "evt_event_hub_00000000000000000002";
    let durable = fixture
        .storage
        .load_outbox_event(event_id)
        .expect("load second durable event")
        .expect("second durable event");
    assert!(
        first_restart
            .publish_committed(durable.event())
            .expect("hub write before outbox acknowledgement")
    );
    let mut before_restart = subscription.events.try_recv().expect("live second frame");
    before_restart["subscriptionId"] = serde_json::json!("sub_crash_exact");
    let stored_before = stored_event_facts(first_restart.database_path(), event_id);
    first_restart
        .close()
        .expect("crash before outbox acknowledgement");

    let second_restart = DurableEventHub::open(
        fixture.root.join("event-hub"),
        DurableEventHubConfig::default(),
    )
    .expect("second restart");
    assert_eq!(
        second_restart
            .publish_pending(&mut fixture.storage)
            .expect("replay pending outbox"),
        0
    );
    assert_eq!(
        stored_event_facts(second_restart.database_path(), event_id),
        stored_before
    );
    let replay = second_restart
        .subscribe(
            &fixture.principal,
            fixture.subscription(
                "sub_crash_exact_after",
                ControlPlaneWebSocketSubscribeStartAt::ControlPlaneWebSocketSubscribeOrigin(
                    ControlPlaneWebSocketSubscribeOrigin::EarliestAvailable,
                ),
            ),
        )
        .expect("subscribe after second restart");
    let mut after_restart = replay.initial_frames[2].clone();
    after_restart["subscriptionId"] = serde_json::json!("sub_crash_exact");
    assert_eq!(
        serde_json::to_vec(&after_restart).expect("replayed frame bytes"),
        serde_json::to_vec(&before_restart).expect("original frame bytes")
    );
    second_restart.close().expect("close second restart");
}

#[test]
fn changed_durable_context_conflicts_without_overwriting_the_hub_event() {
    let mut fixture = Fixture::new(DurableEventHubConfig::default());
    fixture.publish(1);
    let event_id = "evt_event_hub_00000000000000000001";
    let baseline = stored_event_facts(fixture.hub.database_path(), event_id);
    let alternate_root = fixture.root.join("alternate-storage");
    let mut alternate = SqliteStorage::open(&alternate_root).expect("alternate storage");
    let actor = PublicEventActor::User {
        id: UserId("usr_01J00000000000000000000000".into()),
    };
    let payload = serde_json::to_vec(&ControlPlaneWebSocketDeliveryChangedEvent {
        change_kind: "advanced".to_owned(),
        delivery_id: fixture.delivery_id.clone(),
        revision: Revision(1),
        type_value: ControlPlaneWebSocketDeliveryChangedEventTypeValue::DeliveryChangedV1,
    })
    .expect("alternate payload");
    alternate
        .commit(&StateCommit::new(
            ReceiptIdentity::new(
                receipt_actor_key(&actor).expect("alternate actor key"),
                receipt_scope_key(&fixture.public_scope).expect("alternate scope key"),
                RequestId("req_alternate_context_0001".into()),
            )
            .expect("alternate receipt"),
            Sha256Digest(format!("sha256:{}", "e".repeat(64))),
            "alternate-state",
            0,
            b"alternate".to_vec(),
            vec![
                NewOutboxEvent::public_projection(
                    ControlPlaneEventId(event_id.into()),
                    "fixture.event.v1",
                    payload,
                    ProjectionEventStream::Delivery(fixture.delivery_id.clone()),
                    fixture.public_scope.clone(),
                    Instant("2026-08-27T01:00:01.000Z".into()),
                    PublicEventSource::ControlPlane {
                        actor,
                        component: "changed-component".into(),
                    },
                )
                .expect("alternate public event"),
            ],
        ))
        .expect("commit alternate event");
    let alternate_event = alternate
        .load_outbox_event(event_id)
        .expect("load alternate event")
        .expect("alternate event");
    let error = fixture
        .hub
        .publish_committed(alternate_event.event())
        .expect_err("changed durable context conflicts");
    assert_eq!(
        error.code(),
        winwincode_server::DurableEventHubErrorCode::Conflict
    );
    assert_eq!(
        stored_event_facts(fixture.hub.database_path(), event_id),
        baseline
    );
    Box::new(alternate)
        .close()
        .expect("close alternate storage");
}

#[test]
fn durable_ack_and_restart_resume_replay_only_after_the_cursor() {
    let mut fixture = Fixture::new(DurableEventHubConfig::default());
    fixture.publish(1);
    fixture.publish(2);
    let subscription_frame = fixture.subscription(
        "sub_event_hub_resume",
        ControlPlaneWebSocketSubscribeStartAt::ControlPlaneWebSocketSubscribeOrigin(
            ControlPlaneWebSocketSubscribeOrigin::EarliestAvailable,
        ),
    );
    let subscription_spec = match &subscription_frame {
        ControlPlaneWebSocketClientFrame::ControlPlaneWebSocketSubscribeFrame(frame) => {
            frame.subscription.clone()
        }
        _ => unreachable!(),
    };
    let subscription = fixture
        .hub
        .subscribe(&fixture.principal, subscription_frame)
        .expect("subscribe from earliest");
    let event_one: ControlPlaneWebSocketEventFrame =
        serde_json::from_value(subscription.initial_frames[1].clone()).expect("event one");
    assert_eq!(event_one.sequence.0, 1);
    let acknowledged = acknowledged(&event_one);
    fixture
        .hub
        .event_control(
            &fixture.principal,
            ControlPlaneWebSocketClientFrame::ControlPlaneWebSocketAckFrame(
                ControlPlaneWebSocketAckFrame {
                    cursor: acknowledged.clone(),
                    subscription_id: event_one.subscription_id.clone(),
                    type_value: ControlPlaneWebSocketAckFrameTypeValue::TransportAckV1,
                },
            ),
        )
        .expect("persist ack");
    fixture.hub.close().expect("close first hub");
    let restarted = DurableEventHub::open(
        fixture.root.join("event-hub"),
        DurableEventHubConfig::default(),
    )
    .expect("restart event hub");
    let resumed = restarted
        .subscribe(
            &fixture.principal,
            ControlPlaneWebSocketClientFrame::ControlPlaneWebSocketResumeFrame(
                ControlPlaneWebSocketResumeFrame {
                    after: acknowledged,
                    subscription: subscription_spec,
                    subscription_id: event_one.subscription_id,
                    type_value: ControlPlaneWebSocketResumeFrameTypeValue::TransportResumeV1,
                },
            ),
        )
        .expect("resume after restart");
    assert_eq!(
        resumed.initial_frames[0]["type"],
        "transport.resume-accepted.v1"
    );
    assert_eq!(resumed.initial_frames[1]["sequence"], 2);
    restarted.close().expect("close restarted hub");
}

#[test]
fn retention_gap_returns_generated_reset_required() {
    let mut fixture = Fixture::new(DurableEventHubConfig::default());
    fixture.publish(1);
    fixture.publish(2);
    fixture.publish(3);
    let subscription = fixture
        .hub
        .subscribe(
            &fixture.principal,
            fixture.subscription(
                "sub_event_hub_retention_seed",
                ControlPlaneWebSocketSubscribeStartAt::ControlPlaneWebSocketSubscribeOrigin(
                    ControlPlaneWebSocketSubscribeOrigin::EarliestAvailable,
                ),
            ),
        )
        .expect("seed subscription");
    let event_one: ControlPlaneWebSocketEventFrame =
        serde_json::from_value(subscription.initial_frames[1].clone()).expect("event one");
    fixture
        .hub
        .retain_from(&fixture.context.scope, &fixture.context.stream, 3)
        .expect("advance retention");
    let reset = fixture
        .hub
        .subscribe(
            &fixture.principal,
            ControlPlaneWebSocketClientFrame::ControlPlaneWebSocketResumeFrame(
                ControlPlaneWebSocketResumeFrame {
                    after: acknowledged(&event_one),
                    subscription: ControlPlaneWebSocketSubscription {
                        event_types: vec![ControlPlaneWebSocketEventType::DeliveryChangedV1],
                        scope: fixture.context.scope.clone(),
                        stream: fixture.context.stream.clone(),
                    },
                    subscription_id: ControlPlaneWebSocketSubscriptionId(
                        "sub_event_hub_retention".to_owned(),
                    ),
                    type_value: ControlPlaneWebSocketResumeFrameTypeValue::TransportResumeV1,
                },
            ),
        )
        .expect("expired cursor returns reset channel");
    assert_eq!(
        reset.initial_frames[0]["type"],
        "transport.reset-required.v1"
    );
    assert_eq!(reset.initial_frames[0]["closeCode"].as_f64(), Some(4_409.0));
    assert_eq!(reset.initial_frames[0]["earliestAvailable"]["sequence"], 2);
}

#[test]
fn authorization_revoke_closes_existing_authority_epoch() {
    let fixture = Fixture::new(DurableEventHubConfig::default());
    let mut subscription = fixture
        .hub
        .subscribe(
            &fixture.principal,
            fixture.subscription(
                "sub_event_hub_revoke",
                ControlPlaneWebSocketSubscribeStartAt::ControlPlaneWebSocketSubscribeOrigin(
                    ControlPlaneWebSocketSubscribeOrigin::Latest,
                ),
            ),
        )
        .expect("subscribe");
    let frames = fixture
        .hub
        .revoke_authorization(
            &fixture.principal,
            &fixture.context.scope,
            &ControlPlaneWebSocketAuthorizationEpoch(2),
        )
        .expect("revoke authorization");
    assert_eq!(frames[0]["type"], "transport.authorization-revoked.v1");
    assert_eq!(
        subscription.events.try_recv().expect("live revocation")["closeCode"],
        serde_json::json!(4_403.0)
    );
}

#[test]
fn slow_consumer_receives_backpressure_and_ack_releases_durable_replay() {
    let mut fixture = Fixture::new(DurableEventHubConfig::default());
    let mut subscription = fixture
        .hub
        .subscribe(
            &fixture.principal,
            fixture.subscription(
                "sub_event_hub_backpressure",
                ControlPlaneWebSocketSubscribeStartAt::ControlPlaneWebSocketSubscribeOrigin(
                    ControlPlaneWebSocketSubscribeOrigin::Latest,
                ),
            ),
        )
        .expect("subscribe");
    let mut event_one = None;
    for sequence in 1..=256 {
        fixture.publish(sequence);
        let value = subscription.events.try_recv().expect("live event");
        if sequence == 1 {
            event_one = Some(serde_json::from_value(value).expect("typed first event"));
        }
    }
    let event_one: ControlPlaneWebSocketEventFrame = event_one.expect("first event");
    fixture.publish(257);
    let backpressure = subscription.events.try_recv().expect("backpressure frame");
    assert_eq!(backpressure["type"], "transport.backpressure.v1");
    assert_eq!(backpressure["pendingEventCount"], 257);
    assert_eq!(backpressure["ackRequiredThrough"]["sequence"], 256);
    assert_eq!(backpressure["disconnectAt"], "2026-08-27T01:00:30.000Z");
    let expected: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/durable-event-hub.backpressure.valid.json"
    ))
    .expect("contract fixture");
    assert_eq!(
        backpressure["closeCode"].as_f64(),
        expected["closeCode"].as_f64()
    );
    let mut actual_without_close_code = backpressure.clone();
    actual_without_close_code
        .as_object_mut()
        .expect("backpressure object")
        .remove("closeCode");
    let mut expected_without_close_code = expected;
    expected_without_close_code
        .as_object_mut()
        .expect("fixture object")
        .remove("closeCode");
    assert_eq!(actual_without_close_code, expected_without_close_code);
    let replay = fixture
        .hub
        .event_control(
            &fixture.principal,
            ControlPlaneWebSocketClientFrame::ControlPlaneWebSocketAckFrame(
                ControlPlaneWebSocketAckFrame {
                    cursor: acknowledged(&event_one),
                    subscription_id: event_one.subscription_id,
                    type_value: ControlPlaneWebSocketAckFrameTypeValue::TransportAckV1,
                },
            ),
        )
        .expect("ack releases replay");
    assert_eq!(replay.len(), 1);
    assert_eq!(replay[0]["sequence"], 257);
}

#[test]
fn event_type_filter_crosses_more_than_one_replay_window_without_false_backpressure() {
    let mut fixture = Fixture::new(DurableEventHubConfig::default());
    assert_eq!(fixture.publish_task_range(1, 257), 257);
    fixture.publish(258);
    let mut subscription = fixture
        .hub
        .subscribe(
            &fixture.principal,
            fixture.subscription(
                "sub_event_hub_filtered",
                ControlPlaneWebSocketSubscribeStartAt::ControlPlaneWebSocketSubscribeOrigin(
                    ControlPlaneWebSocketSubscribeOrigin::EarliestAvailable,
                ),
            ),
        )
        .expect("subscribe to filtered stream");
    assert_eq!(subscription.initial_frames.len(), 2);
    assert_eq!(subscription.initial_frames[1]["sequence"], 258);

    assert_eq!(fixture.publish_task_range(259, 515), 257);
    fixture.publish(516);
    let matching = subscription
        .events
        .try_recv()
        .expect("matching event is not starved or backpressured");
    assert_eq!(matching["type"], "event.v1");
    assert_eq!(matching["sequence"], 516);
}

#[test]
fn full_live_channel_disconnects_before_a_sent_cursor_can_cross_a_hole() {
    let mut fixture = Fixture::new(DurableEventHubConfig::default());
    let subscription_spec = ControlPlaneWebSocketSubscription {
        event_types: vec![ControlPlaneWebSocketEventType::DeliveryChangedV1],
        scope: fixture.context.scope.clone(),
        stream: fixture.context.stream.clone(),
    };
    let subscription_id =
        ControlPlaneWebSocketSubscriptionId("sub_event_hub_full_channel".to_owned());
    let mut subscription = fixture
        .hub
        .subscribe(
            &fixture.principal,
            fixture.subscription(
                &subscription_id.0,
                ControlPlaneWebSocketSubscribeStartAt::ControlPlaneWebSocketSubscribeOrigin(
                    ControlPlaneWebSocketSubscribeOrigin::Latest,
                ),
            ),
        )
        .expect("subscribe");
    fixture.publish(1);
    let event_one: ControlPlaneWebSocketEventFrame =
        serde_json::from_value(subscription.events.try_recv().expect("first event"))
            .expect("typed first event");
    let acknowledged_one = acknowledged(&event_one);
    assert!(
        fixture
            .hub
            .event_control(
                &fixture.principal,
                ControlPlaneWebSocketClientFrame::ControlPlaneWebSocketAckFrame(
                    ControlPlaneWebSocketAckFrame {
                        cursor: acknowledged_one.clone(),
                        subscription_id: subscription_id.clone(),
                        type_value: ControlPlaneWebSocketAckFrameTypeValue::TransportAckV1,
                    },
                ),
            )
            .expect("ack first event")
            .is_empty()
    );
    let payloads = (2..=257)
        .map(|sequence| {
            let event = ControlPlaneWebSocketDeliveryChangedEvent {
                change_kind: "advanced".to_owned(),
                delivery_id: fixture.delivery_id.clone(),
                revision: Revision(i64::from(sequence)),
                type_value: ControlPlaneWebSocketDeliveryChangedEventTypeValue::DeliveryChangedV1,
            };
            (
                u64::try_from(sequence).expect("sequence"),
                serde_json::to_vec(&event).expect("event payload"),
            )
        })
        .collect();
    assert_eq!(fixture.publish_payloads(payloads), 256);
    fixture.publish(258);

    let resumed = fixture
        .hub
        .subscribe(
            &fixture.principal,
            ControlPlaneWebSocketClientFrame::ControlPlaneWebSocketResumeFrame(
                ControlPlaneWebSocketResumeFrame {
                    after: acknowledged_one,
                    subscription: subscription_spec,
                    subscription_id: subscription_id.clone(),
                    type_value: ControlPlaneWebSocketResumeFrameTypeValue::TransportResumeV1,
                },
            ),
        )
        .expect("resume from last durable ack");
    assert_eq!(resumed.initial_frames.len(), 257);
    assert_eq!(resumed.initial_frames[1]["sequence"], 2);
    assert_eq!(resumed.initial_frames[256]["sequence"], 257);
    let event_257: ControlPlaneWebSocketEventFrame =
        serde_json::from_value(resumed.initial_frames[256].clone()).expect("last replay event");
    let released = fixture
        .hub
        .event_control(
            &fixture.principal,
            ControlPlaneWebSocketClientFrame::ControlPlaneWebSocketAckFrame(
                ControlPlaneWebSocketAckFrame {
                    cursor: acknowledged(&event_257),
                    subscription_id,
                    type_value: ControlPlaneWebSocketAckFrameTypeValue::TransportAckV1,
                },
            ),
        )
        .expect("ack replay window");
    assert_eq!(released.len(), 1);
    assert_eq!(released[0]["sequence"], 258);
}

#[test]
fn public_errors_redact_storage_and_corruption_diagnostics() {
    let suffix = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "winwincode-event-hub-errors-{}-{suffix}",
        std::process::id()
    ));
    fs::create_dir_all(&root).expect("error fixture root");
    let file_path = root.join("not-a-directory");
    fs::write(&file_path, b"fixture").expect("fixture file");
    let storage_error = DurableEventHub::open(&file_path, DurableEventHubConfig::default())
        .err()
        .expect("storage failure");
    assert!(storage_error.to_string().contains("event-hub directory"));
    let public = storage_error.api_error();
    assert_eq!(public.code(), "SERVICE_UNAVAILABLE");
    assert_eq!(public.message(), "event service is temporarily unavailable");
    assert!(!public.message().contains("directory"));

    let corrupt_root = root.join("corrupt");
    fs::create_dir_all(&corrupt_root).expect("corrupt root");
    let connection =
        Connection::open(corrupt_root.join("server-event-hub.sqlite3")).expect("corrupt database");
    connection
        .execute_batch(
            "CREATE TABLE hub_meta(key TEXT PRIMARY KEY NOT NULL,value INTEGER NOT NULL);\
             INSERT INTO hub_meta(key,value) VALUES('schema_version',999);",
        )
        .expect("corrupt schema version");
    drop(connection);
    let corrupt_error = DurableEventHub::open(&corrupt_root, DurableEventHubConfig::default())
        .err()
        .expect("corrupt failure");
    assert!(corrupt_error.to_string().contains("schema version"));
    let public = corrupt_error.api_error();
    assert_eq!(public.code(), "INTERNAL_ERROR");
    assert_eq!(public.message(), "event service failed");
    assert!(!public.message().contains("schema"));
    fs::remove_dir_all(root).expect("remove error fixtures");
}

#[test]
fn transport_limits_reject_values_outside_the_canonical_contract() {
    let suffix = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "winwincode-event-hub-limits-{}-{suffix}",
        std::process::id()
    ));
    let config = DurableEventHubConfig {
        max_unacked_events: 1,
        ..DurableEventHubConfig::default()
    };
    let error = DurableEventHub::open(&root, config)
        .err()
        .expect("non-contract limits are rejected");
    assert_eq!(error.api_error().code(), "INVALID_REQUEST");
    assert!(!root.exists());
}

fn acknowledged(
    event: &ControlPlaneWebSocketEventFrame,
) -> ControlPlaneWebSocketAcknowledgedCursor {
    ControlPlaneWebSocketAcknowledgedCursor {
        event_id: event.event_id.clone(),
        scope: event.scope.clone(),
        sequence: event.sequence.clone(),
        stream: event.stream.clone(),
    }
}

fn stored_event_facts(database_path: &std::path::Path, event_id: &str) -> Vec<u8> {
    let connection = Connection::open(database_path).expect("open event hub database");
    let facts = connection
        .query_row(
            "SELECT event_id,scope_json,stream_json,sequence,topic,event_type_json,\
             payload_json,occurred_at_json,source_json FROM hub_events WHERE event_id=?1",
            [event_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                ))
            },
        )
        .expect("stored event facts");
    serde_json::to_vec(&facts).expect("stored event facts JSON")
}
