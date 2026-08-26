use winwincode_delivery::domain::{
    Delivery, DeliveryValidationErrorCode, delivery_id_for_github_issue_source,
};

const CANONICAL_DELIVERY_ID: &str = "dlv_7TEPT1B6JF7W5SASWZMKTCC4KT";

fn github_delivery_snapshot(delivery_id: &str) -> serde_json::Value {
    fn replace_delivery_ids(value: &mut serde_json::Value, delivery_id: &str) {
        match value {
            serde_json::Value::Array(values) => {
                for value in values {
                    replace_delivery_ids(value, delivery_id);
                }
            }
            serde_json::Value::Object(object) => {
                if object.contains_key("deliveryId") {
                    object.insert(
                        "deliveryId".to_owned(),
                        serde_json::Value::String(delivery_id.to_owned()),
                    );
                }
                for value in object.values_mut() {
                    replace_delivery_ids(value, delivery_id);
                }
            }
            _ => {}
        }
    }

    let mut snapshot: serde_json::Value =
        serde_json::from_slice(include_bytes!("fixtures/delivery-main.json"))
            .expect("fixture json");
    replace_delivery_ids(&mut snapshot, delivery_id);
    snapshot["id"] = serde_json::json!(delivery_id);
    snapshot["spec"]["sourceRef"] = serde_json::json!({
        "schemaVersion": 3,
        "provider": "github",
        "kind": "issue",
        "repository": "Example/Widget",
        "number": 42
    });
    snapshot["spec"]["publicationTarget"] = serde_json::json!({
        "schemaVersion": 3,
        "provider": "github",
        "kind": "pull-request",
        "repository": "example/widget",
        "baseBranch": "main",
        "headRepository": "contributor/widget",
        "headBranch": "winwincode/issue-42"
    });
    snapshot["spec"]["repository"] = serde_json::json!({
        "schemaVersion": 3,
        "kind": "github",
        "locator": "example/widget"
    });
    snapshot
}

#[test]
fn canonical_typescript_fixture_round_trips() {
    let fixture = include_bytes!("fixtures/delivery-main.json");
    let delivery = Delivery::decode_json(fixture).expect("the current TypeScript fixture is valid");

    assert_eq!(delivery.id().0, "dlv_01J00000000000000000000000");
    assert_eq!(delivery.revision(), 7);
    assert_eq!(
        Delivery::decode_json(&delivery.encode_json().expect("encode"))
            .expect("decode")
            .snapshot(),
        delivery.snapshot()
    );
}

#[test]
fn canonical_delivery_id_keeps_an_exact_github_source_and_cross_repository_target() {
    let snapshot = github_delivery_snapshot(CANONICAL_DELIVERY_ID);
    let delivery = Delivery::decode_json(&serde_json::to_vec(&snapshot).expect("snapshot json"))
        .expect("canonical Delivery identity is independent from its GitHub source");

    assert_eq!(delivery.id().0, CANONICAL_DELIVERY_ID);
    let source = delivery
        .snapshot()
        .spec
        .source_ref
        .as_ref()
        .expect("GitHub source");
    assert_eq!(source.repository, "example/widget");
    assert_eq!(source.number, 42);
    assert_eq!(
        delivery_id_for_github_issue_source(source)
            .expect("stable GitHub source identity")
            .0,
        CANONICAL_DELIVERY_ID
    );
    let target = delivery
        .snapshot()
        .spec
        .publication_target
        .as_ref()
        .expect("pull request target");
    assert_eq!(target.repository, "example/widget");
    assert_eq!(target.head_repository, "contributor/widget");
}

#[test]
fn github_source_rejects_an_unrelated_canonical_delivery_id() {
    let snapshot = github_delivery_snapshot("dlv_01J00000000000000000000000");
    let error = Delivery::decode_json(&serde_json::to_vec(&snapshot).expect("snapshot json"))
        .expect_err("one GitHub issue must converge on one Delivery identity");

    assert_eq!(
        error.code(),
        DeliveryValidationErrorCode::RelationshipMismatch
    );
    assert_eq!(error.path(), "delivery.spec.deliveryId");
}

#[test]
fn rust_github_delivery_id_matches_the_cross_language_fixed_vectors() {
    for (repository, number, expected) in [
        ("Example/Widget", 42, "dlv_7TEPT1B6JF7W5SASWZMKTCC4KT"),
        ("example/widget", 43, "dlv_5PXTV8B1HJ69R5CM3WBYM5APSG"),
        ("contributor/widget", 42, "dlv_6WTC6MD0AZX57FVY2JT32QG9HF"),
    ] {
        let source = winwincode_delivery::domain::GitHubIssueSourceRef {
            schema_version: 3,
            provider: "github".into(),
            kind: "issue".into(),
            repository: repository.into(),
            number,
        };
        assert_eq!(
            delivery_id_for_github_issue_source(&source)
                .expect("valid GitHub issue source")
                .0,
            expected
        );
    }
}

#[test]
fn github_issue_identity_is_not_a_delivery_id() {
    let snapshot = github_delivery_snapshot("github-issue:example/widget:42");
    let error = Delivery::decode_json(&serde_json::to_vec(&snapshot).expect("snapshot json"))
        .expect_err("the retired GitHub-derived identity must be rejected");

    assert_eq!(error.code(), DeliveryValidationErrorCode::InvalidIdentifier);
    assert_eq!(error.path(), "delivery.id");
}

#[test]
fn delivery_spec_requires_required_acceptance_criterion() {
    let fixture = include_bytes!("fixtures/delivery-main-no-required.json");
    let error = Delivery::decode_json(fixture).expect_err("required criterion must be enforced");
    assert_eq!(error.code(), DeliveryValidationErrorCode::InvalidValue);
}
