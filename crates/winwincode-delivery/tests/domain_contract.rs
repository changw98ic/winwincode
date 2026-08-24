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
