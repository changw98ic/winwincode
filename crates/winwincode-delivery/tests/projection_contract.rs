// SPDX-License-Identifier: Apache-2.0

use winwincode_delivery::{
    domain::Delivery,
    projection::{ProjectionInput, project_delivery_detail},
};

fn delivery_without_candidate_facts() -> Delivery {
    let mut value: serde_json::Value =
        serde_json::from_slice(include_bytes!("fixtures/delivery-main.json"))
            .expect("Delivery fixture JSON");
    value["status"] = serde_json::json!("verifying");
    value["evidence"] = serde_json::json!([]);
    value["verdict"] = serde_json::Value::Null;
    Delivery::decode_json(&serde_json::to_vec(&value).expect("Delivery JSON"))
        .expect("current Delivery")
}

#[test]
fn public_projection_seam_returns_values_instead_of_the_delivery_entity() {
    let delivery = delivery_without_candidate_facts();

    let projection = project_delivery_detail(ProjectionInput::new(&delivery))
        .expect("read-only detail projection");

    assert_eq!(projection.delivery_id(), delivery.id());
    assert_eq!(projection.delivery_revision(), delivery.revision());
    assert_eq!(
        projection.requirements().spec().title(),
        "Implement invitation flow"
    );
    assert!(projection.evidence().is_empty());
    assert!(projection.verdict().is_none());
    assert_ne!(
        serde_json::to_value(&projection).expect("projection JSON"),
        serde_json::to_value(&delivery).expect("Delivery JSON")
    );
}
