// SPDX-License-Identifier: Apache-2.0

//! `StrongFlow` source reconstruction must allow canonical Worker ingress to progress.

#![cfg(feature = "local-worker")]

#[path = "support/strongflow_query_progress.rs"]
mod support;

use std::{sync::mpsc, thread, time::Duration};

use winwincode_control_plane::{
    DurableExecutionPortContext, DurableExecutionPortDelegate, DurableExecutionPortError,
    DurableExecutionPortSupplement,
};
use winwincode_execution_port::{
    generated::{ExecutionPortMessage, WorkerHeartbeatAckMessageStatus},
    transport::ExecutionPortCore,
};
use winwincode_server::{ServerExecutionPortCore, TypedControlPlaneApiPort};

struct NoJobMessages;

#[test]
fn candidate_history_read_does_not_recover_a_missing_retention_reference() {
    let fixture = support::Fixture::new();
    fixture.gate.release();
    let current = fixture
        .application
        .query(
            &fixture.principal(),
            winwincode_server::QueryFamily::Delivery,
            fixture.delivery_get(None, 91_000),
        )
        .expect("real current Candidate");
    let current = serde_json::to_value(current).unwrap();
    let cursor = &current["result"]["readCursor"];
    let warm = fixture
        .application
        .query(
            &fixture.principal(),
            winwincode_server::QueryFamily::Delivery,
            fixture.candidate_history(cursor, 91_001),
        )
        .expect("history has exact settled retention");
    let warm = serde_json::to_value(warm).unwrap();
    assert_eq!(
        warm["result"]["items"]
            .as_array()
            .expect("history items")
            .len(),
        1
    );
    fixture.remove_pin();
    assert!(fixture.pin_is_missing());
    let missing = fixture.application.query(
        &fixture.principal(),
        winwincode_server::QueryFamily::Delivery,
        fixture.candidate_history(cursor, 91_002),
    );
    fixture
        .application
        .shutdown()
        .expect("application shutdown");
    assert!(
        missing.is_err(),
        "history must fail closed instead of recovering a missing pin"
    );
    assert!(
        fixture.pin_is_missing(),
        "readonly history must never restore the pin or run retention recovery"
    );
}

impl DurableExecutionPortDelegate for NoJobMessages {
    fn accept(
        &mut self,
        _context: DurableExecutionPortContext<'_>,
        _supplement: DurableExecutionPortSupplement<'_>,
    ) -> Result<Vec<ExecutionPortMessage>, DurableExecutionPortError> {
        panic!("the heartbeat must use the canonical ingress owner");
    }
}

#[test]
fn slow_candidate_resolution_does_not_block_independent_worker_ingress() {
    let fixture = support::Fixture::new();
    let mut ingress = ServerExecutionPortCore::from_application(
        &fixture.application,
        fixture.scope.clone(),
        NoJobMessages,
        Duration::from_secs(30),
    )
    .expect("canonical Server ingress");
    let query_application = std::sync::Arc::clone(&fixture.application);
    let query = fixture.delivery_get(None, 90_001);
    let principal = fixture.principal();
    let query_started = std::time::Instant::now();
    let query_thread = thread::spawn(move || {
        query_application
            .query(&principal, winwincode_server::QueryFamily::Delivery, query)
            .map(|response| serde_json::to_value(response).expect("query response JSON"))
            .map_err(|error| error.to_string())
    });

    // This barrier is reached only by resolving the real stored Git Artifact.
    // It deliberately holds filesystem work, rather than an API test double.
    let entered = fixture.gate.wait_until_entered(Duration::from_secs(10));
    if !entered {
        fixture.gate.release();
        let result = query_thread.join().expect("query thread");
        panic!("the production source resolver was not reached: {result:?}");
    }
    let heartbeat = fixture.heartbeat();
    let (ingress_finished, ingress_result) = mpsc::channel();
    let ingress_thread = thread::spawn(move || {
        let started = std::time::Instant::now();
        let result = ingress.accept(&heartbeat);
        ingress_finished
            .send((started.elapsed(), result))
            .expect("report ingress outcome");
    });

    // A bounded watchdog observes ordering. The source barrier remains closed
    // throughout this wait, so a successful ACK proves independent progress.
    let before_release = ingress_result.recv_timeout(Duration::from_secs(1));
    let completed_before_release = before_release.is_ok();
    fixture.gate.release();
    let (ingress_elapsed, accepted) = before_release.unwrap_or_else(|_| {
        ingress_result
            .recv_timeout(Duration::from_secs(10))
            .expect("ingress finishes after the source barrier releases")
    });
    ingress_thread.join().expect("ingress thread");
    let query_result = query_thread.join().expect("query thread");
    let query_elapsed = query_started.elapsed();
    // Re-enter through the same production Application after ingress has
    // advanced. Exact replay must retain the original response's whole cut.
    let exact_result = match &query_result {
        Ok(response) => fixture
            .application
            .query(
                &fixture.principal(),
                winwincode_server::QueryFamily::Delivery,
                fixture.delivery_get(Some(&response["result"]["readCursor"]), 90_002),
            )
            .map(|response| serde_json::to_value(response).expect("exact query response JSON"))
            .map_err(|error| error.to_string()),
        Err(error) => Err(error.clone()),
    };
    fixture
        .application
        .shutdown()
        .expect("application shutdown");
    eprintln!(
        "{}",
        serde_json::json!({
            "regression": "o8bl-slow-git-vs-canonical-heartbeat",
            "queryElapsedMs": query_elapsed.as_millis(),
            "workerIngressElapsedMs": ingress_elapsed.as_millis(),
            "ingressCompletedBeforeGitRelease": completed_before_release,
            "querySucceeded": query_result.is_ok(),
            "queryError": query_result.as_ref().err(),
            "ingressSucceeded": accepted.is_ok()
        })
    );
    assert!(
        completed_before_release && query_result.is_ok() && accepted.is_ok(),
        "StrongFlow/ingress outcomes: ingress_completed_before_release={completed_before_release}, \
         ingress_elapsed={ingress_elapsed:?}, query_elapsed={query_elapsed:?}, \
         query={query_result:?}, ingress={accepted:?}"
    );
    let response = query_result.expect("query success checked above");
    let accepted = accepted.expect("ingress success checked above");
    let [ExecutionPortMessage::WorkerHeartbeatAckMessage(ack)] = accepted.as_slice() else {
        panic!("canonical heartbeat ACK expected, got {accepted:?}");
    };
    assert_eq!(ack.status, WorkerHeartbeatAckMessageStatus::Accepted);
    assert_eq!(ack.worker_id, fixture.worker_id);
    assert_eq!(ack.heartbeat_sequence.0, 1);
    assert_eq!(response["result"]["deliveryId"], fixture.delivery_id.0);
    assert!(response["result"]["readCursor"].is_object(), "{response}");
    assert!(
        response["result"]["currentCandidate"].is_object(),
        "{response}"
    );
    let exact = exact_result.expect("exact replay succeeds");
    assert_eq!(
        exact["result"], response["result"],
        "current and exact reads must share the complete bounded cut"
    );
}
