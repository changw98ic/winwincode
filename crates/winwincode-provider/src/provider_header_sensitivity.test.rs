// SPDX-License-Identifier: Apache-2.0

fn metadata_header_settings(fixture: &TlsFixture) -> HttpsSseProviderConfig {
    config(fixture)
        .with_custom_headers([
            ("X-Title".into(), "My App".into()),
            (
                "X-Private-Provider-Header".into(),
                "fixture-private-header-value".into(),
            ),
            (
                "X-Unknown-Provider-Header".into(),
                "fixture-unknown-header-value".into(),
            ),
        ])
        .unwrap()
}

fn metadata_header_payload() -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "requestId":"header-request", "provider":"provider-https-fixture",
        "sessionId":"header-session", "threadId":"header-thread",
        "request":{"model":"fixture-model", "instructions":"Reply briefly",
            "input":[{"type":"message", "role":"user", "content":[{"type":"input_text", "text":"hello"}]}],
            "stream":true, "store":false, "tool_choice":"none", "parallel_tool_calls":false}
    }))
    .unwrap()
}

fn metadata_header_sse(text: &str) -> String {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        serde_json::json!({"id":"header-response", "model":"fixture-model",
            "choices":[{"index":0, "delta":{"content":text}, "finish_reason":"stop"}],
            "usage":{"prompt_tokens":2, "completion_tokens":1}})
    )
}

fn metadata_header_echoes() -> [&'static str; 4] {
    [
        "My App",
        std::str::from_utf8(SECRET).unwrap(),
        "fixture-private-header-value",
        "fixture-unknown-header-value",
    ]
}

fn metadata_header_output(
    chunks: &[winwincode_execution_port::generated::ModelChunkMessage],
) -> String {
    use base64::{Engine as _, engine::general_purpose::STANDARD};

    let mut output = String::new();
    for chunk in chunks {
        if let Some(payload) = &chunk.payload {
            output.push_str(
                &String::from_utf8(STANDARD.decode(&payload.data_base64).unwrap()).unwrap(),
            );
        }
        if let Some(error) = &chunk.error {
            output.push_str(&error.message);
        }
    }
    output
}

#[test]
fn metadata_header_stream_preserves_public_text_and_blocks_credentials() {
    for echoed in metadata_header_echoes() {
        let body = metadata_header_sse(echoed);
        let fixture = TlsFixture::start(vec![TestResponse {
            status: "200 OK",
            content_type: "text/event-stream",
            body: &body,
            declared_length: None,
            delay: Duration::ZERO,
        }]);
        let adapter = HttpsSseProviderAdapter::try_new(
            metadata_header_settings(&fixture)
                .with_openai_chat_completions(8192, ProviderTokenPricing::default())
                .unwrap(),
        )
        .unwrap();
        let exchange = ModelExchangeId("mdl_00000000000000000000000001".into());
        let request_id = RequestId("req_00000000000000000000000001".into());
        let payload = metadata_header_payload();
        let mut request = invocation(&exchange, &request_id);
        request.payload = &payload;
        adapter.open_https(request, SECRET).unwrap();
        let result = adapter.drain_canonical(&responses_api_receipt(exchange, request_id));
        assert_eq!(fixture.finish().len(), 1);
        if echoed == "My App" {
            let completion = result.unwrap();
            assert!(matches!(
                completion.terminal,
                ProviderGatewayTerminal::Completed { .. }
            ));
            assert!(
                completion
                    .frames
                    .iter()
                    .any(|frame| frame.payload_json().contains("My App"))
            );
        } else {
            assert_eq!(
                result.unwrap_err().kind(),
                HttpsSseProviderErrorKind::CredentialLeak
            );
        }
    }
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one real model flow compares public metadata and all credential output controls"
)]
fn metadata_header_device_model_preserves_public_text_and_blocks_credentials() {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use winwincode_execution_port::generated::ExecutionPortErrorCode;

    for echoed in metadata_header_echoes() {
        let body = metadata_header_sse(echoed);
        let fixture = TlsFixture::start(vec![TestResponse {
            status: "200 OK",
            content_type: "text/event-stream",
            body: &body,
            declared_length: None,
            delay: Duration::ZERO,
        }]);
        let directory = HttpErrorLogDirectory::new();
        let store = crate::DeviceProviderStore::open(&directory.0.join("providers")).unwrap();
        let provider = serde_json::json!({
            "providerId":"provider-https-fixture", "displayName":"fixture",
            "endpoint":fixture.endpoint, "protocol":"openai_chat_completions",
            "modelIds":["fixture-model"], "enabled":true
        });
        store
            .connection
            .execute(
                "INSERT INTO providers VALUES(?1,?2,?3)",
                rusqlite::params!["provider-https-fixture", provider.to_string(), SECRET],
            )
            .unwrap();
        let headers = metadata_header_settings(&fixture)
            .custom_headers
            .into_iter()
            .collect::<BTreeMap<_, _>>();
        store
            .connection
            .execute(
                "INSERT INTO provider_headers VALUES(?1,?2)",
                rusqlite::params![
                    "provider-https-fixture",
                    serde_json::to_string(&headers).unwrap()
                ],
            )
            .unwrap();
        let template: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/contracts/execution-port.valid.json"
        ))
        .unwrap();
        let mut open: winwincode_execution_port::generated::ModelOpenMessage =
            serde_json::from_value(
                template["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|message| message["kind"] == "model.open")
                    .unwrap()
                    .clone(),
            )
            .unwrap();
        let payload = metadata_header_payload();
        open.request.data_base64 = STANDARD.encode(&payload);
        open.request.content_type = "application/json".into();
        open.request.payload_digest.0 = format!("sha256:{:x}", Sha256::digest(&payload));
        let runtime = crate::ProviderRuntimeStore::prepare(&directory.0.join("providers")).unwrap();
        let chunks = runtime
            .execute_model_using(
                &open,
                || true,
                |_, headers| {
                    let settings = config(&fixture)
                        .with_openai_chat_completions(8192, ProviderTokenPricing::default())
                        .unwrap()
                        .with_custom_headers(headers)
                        .unwrap();
                    HttpsSseProviderAdapter::try_new(settings)
                        .map_err(|_| crate::DeviceProviderError::StateInvalid)
                },
            )
            .unwrap();
        assert_eq!(fixture.finish().len(), 1);
        assert!(chunks.last().unwrap().is_final);
        let output = metadata_header_output(&chunks);
        for credential in metadata_header_echoes().into_iter().skip(1) {
            assert!(
                !output.contains(credential),
                "provider credential reached model output"
            );
        }
        if echoed == "My App" {
            assert!(chunks.iter().all(|chunk| chunk.error.is_none()));
            assert!(output.contains("My App"));
        } else {
            let error = chunks.last().unwrap().error.as_ref().unwrap();
            assert_eq!(
                error.code,
                ExecutionPortErrorCode::DeviceProviderCredentialLeakBlocked
            );
            let (machine_code, network) = error.message.split_once(";network=").unwrap();
            assert_eq!(machine_code, "DEVICE_PROVIDER_CREDENTIAL_LEAK_BLOCKED");
            let network: winwincode_network::NetworkFailure =
                serde_json::from_str(network).unwrap();
            assert_eq!(
                network.kind,
                winwincode_network::ErrorKind::IntegrityInvalid
            );
            assert!(!error.retryable);
        }
    }
}

#[test]
fn metadata_header_http_error_log_preserves_public_text_and_blocks_credentials() {
    for echoed in metadata_header_echoes() {
        let fixture = TlsFixture::start(vec![TestResponse {
            status: "400 Bad Request",
            content_type: "text/plain",
            body: echoed,
            declared_length: None,
            delay: Duration::ZERO,
        }]);
        let directory = HttpErrorLogDirectory::new();
        let failure =
            fixture_http_failure(&fixture, &directory, metadata_header_settings(&fixture));
        assert_eq!(fixture.finish().len(), 1);
        let (metadata, retained) = retained_http_error(&directory, failure);
        let public = echoed == "My App";
        assert_eq!(metadata["bodyRetained"], public);
        assert_eq!(metadata["responseBytesReceived"], echoed.len());
        if public {
            assert_eq!(retained, echoed.as_bytes());
        } else {
            assert!(retained.is_empty());
        }
    }
}
