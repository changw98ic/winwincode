// SPDX-License-Identifier: Apache-2.0
use super::tests::{event_with_fact, fixture};
use super::*;
#[test]
fn long_runs_keep_the_latest_bounded_activity_window_without_losing_the_cursor() {
    let (delivery, mut binding, template) = fixture();
    binding.settled_last_sequence = Some(150);
    binding.seal = seal_binding(&binding).unwrap();
    let mut projection = RuntimeProjection::new(&delivery, vec![binding]).unwrap();
    for sequence in 1..=150 {
        let event = event_with_fact(
            &template,
            sequence,
            AcceptedRuntimeFact::Activity(RuntimeActivityProjection {
                core_tool: None,
                call_id: format!("call-{sequence}"),
                activity_type: RuntimeActivityType::Tool,
                command: Some("trusted read".into()),
                status: RuntimeActivityStatus::Completed,
                outcome: RuntimeActivityOutcome::Observed,
                exit_code: None,
                source_ref: format!("runtime:call-{sequence}"),
            }),
        );
        projection.apply(&event).unwrap();
    }
    let snapshot = projection.snapshot();
    let session = &snapshot.sessions[0];
    assert_eq!(session.as_of_sequence, 150);
    assert_eq!(session.activities.len(), 100);
    assert_eq!(session.activities.first().unwrap().call_id, "call-51");
    assert_eq!(session.activities.last().unwrap().call_id, "call-150");
}
