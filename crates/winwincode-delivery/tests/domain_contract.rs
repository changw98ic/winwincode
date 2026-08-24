use winwincode_delivery::domain::{Delivery, DeliveryValidationErrorCode};

#[test]
fn canonical_typescript_fixture_round_trips() {
    let fixture = include_bytes!("fixtures/delivery-main.json");
    let delivery = Delivery::decode_json(fixture).expect("the current TypeScript fixture is valid");

    assert_eq!(delivery.id().0, "delivery-main");
    assert_eq!(delivery.revision(), 7);
    assert_eq!(
        Delivery::decode_json(&delivery.encode_json().expect("encode"))
            .expect("decode")
            .snapshot(),
        delivery.snapshot()
    );
}

#[test]
fn delivery_spec_requires_required_acceptance_criterion() {
    let fixture = include_bytes!("fixtures/delivery-main-no-required.json");
    let error = Delivery::decode_json(fixture).expect_err("required criterion must be enforced");
    assert_eq!(error.code(), DeliveryValidationErrorCode::InvalidValue);
}

#[test]
fn legacy_typescript_oracle_snapshots_round_trip_through_rust_domain() {
    let oracle_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/oracles/delivery-strongflow-typescript.v1.json");
    let oracle: serde_json::Value = serde_json::from_slice(
        &std::fs::read(oracle_path).expect("committed TypeScript Delivery oracle"),
    )
    .expect("oracle json");
    let scenarios = oracle["scenarios"].as_array().expect("oracle scenarios");
    assert_eq!(scenarios.len(), 10);

    for scenario in scenarios {
        let id = scenario["id"].as_str().expect("scenario id");
        let expected = scenario["observation"]["snapshot"].clone();
        let delivery =
            Delivery::decode_json(&serde_json::to_vec(&expected).expect("snapshot json"))
                .unwrap_or_else(|error| {
                    panic!("TypeScript oracle scenario {id} was rejected: {error}")
                });
        let actual: serde_json::Value =
            serde_json::from_slice(&delivery.encode_json().expect("Rust snapshot json"))
                .expect("encoded snapshot");
        assert_eq!(actual, expected, "scenario {id}");
    }
}
