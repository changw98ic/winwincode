// SPDX-License-Identifier: Apache-2.0

use super::*;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one read-interleaving matrix checks presentation changes and authorization revocation"
)]
fn connection_snapshot_updates_follow_active_route_and_authorization() {
    let root = std::env::temp_dir().join(format!(
        "wwc-connection-snapshot-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let mut results = Vec::new();
    for field in [
        "display",
        "other-model",
        "disabled",
        "selected-model",
        "endpoint",
        "protocol",
        "provider",
        "structured-output",
        "secret",
    ] {
        let directory = root.join(field);
        let store = DeviceProviderStore::open(&directory).unwrap();
        let config = DeviceProviderConfig {
            display_name: "Original name".into(),
            enabled: true,
            endpoint: "https://provider.example/v1/responses".into(),
            model_ids: vec!["selected".into()],
            protocol: DeviceProviderProtocol::OpenaiResponses,
            provider_id: "fixture".into(),
            responses_structured_output: None,
        };
        let original_secret = b"snapshot-original-secret".to_vec();
        store
            .connection
            .execute(
                "INSERT INTO providers(provider_id,config,secret) VALUES(?1,?2,?3)",
                rusqlite::params![
                    config.provider_id,
                    serde_json::to_string(&config).unwrap(),
                    original_secret
                ],
            )
            .unwrap();
        let mut changed = serde_json::to_value(&config).unwrap();
        let secret = match field {
            "secret" => b"snapshot-replaced-secret".to_vec(),
            _ => original_secret.clone(),
        };
        match field {
            "display" => changed["displayName"] = serde_json::json!("Current name"),
            "other-model" => changed["modelIds"] = serde_json::json!(["selected", "another"]),
            "disabled" => changed["enabled"] = serde_json::json!(false),
            "selected-model" => changed["modelIds"] = serde_json::json!(["another"]),
            "endpoint" => {
                changed["endpoint"] = serde_json::json!("https://changed.example/v1/responses");
            }
            "protocol" => changed["protocol"] = serde_json::json!("anthropic_messages"),
            "provider" => changed["providerId"] = serde_json::json!("changed"),
            "structured-output" => {
                changed["responsesStructuredOutput"] = serde_json::json!("json_schema");
            }
            _ => {}
        }
        let saved_config = serde_json::to_string(&changed).unwrap();
        let path = store.connection.path().unwrap().to_owned();
        let reads = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&reads);
        store
            .connection
            .authorizer(Some(move |context: AuthContext<'_>| {
                if matches!(
                    context.action,
                    AuthAction::Read {
                        table_name: "providers",
                        column_name: "config"
                    }
                ) && observed.fetch_add(1, Ordering::SeqCst) == 1
                {
                    rusqlite::Connection::open(&path)
                        .unwrap()
                        .execute(
                            "UPDATE providers SET config=?1,secret=?2 WHERE provider_id='fixture'",
                            rusqlite::params![saved_config, secret],
                        )
                        .unwrap();
                }
                Authorization::Allow
            }))
            .unwrap();
        let result = store.execution_connection(&ProviderExecutionRequest {
            context: None,
            provider_id: "fixture".into(),
            model_id: Some("selected".into()),
            purpose: ProviderExecutionPurpose::Model,
            physical_attempt: None,
            adapter_request_id: None,
        });
        assert_eq!(
            reads.load(Ordering::SeqCst),
            2,
            "{field} update occurs between both configuration reads"
        );
        if let Ok(material) = &result {
            assert_eq!(material.secret.as_deref(), Some(original_secret.as_slice()));
            let ProviderConnectionParameters::Model(parameters) = &material.parameters else {
                panic!("model parameters required")
            };
            assert_eq!(parameters.endpoint, config.endpoint);
            assert_eq!(parameters.provider_id, config.provider_id);
        }
        results.push((field, result.is_ok()));
    }
    std::fs::remove_dir_all(root).unwrap();
    assert_eq!(
        results,
        vec![
            ("display", true),
            ("other-model", true),
            ("disabled", false),
            ("selected-model", false),
            ("endpoint", false),
            ("protocol", false),
            ("provider", false),
            ("structured-output", false),
            ("secret", false)
        ]
    );
}
