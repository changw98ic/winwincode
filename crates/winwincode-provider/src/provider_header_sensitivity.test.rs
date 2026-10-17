// SPDX-License-Identifier: Apache-2.0

fn metadata_header_settings(fixture: &TlsFixture) -> HttpsSseProviderConfig {
    config(fixture)
        .with_custom_headers([
            ("X-Title".into(), "My App".into()),
            ("HTTP-Referer".into(), "https://public.example/app".into()),
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

fn metadata_header_echoes() -> [(&'static str, bool); 8] {
    [
        ("My App", true),
        ("https://public.example/app", true),
        ("Authorization: Bearer $OPENAI_API_KEY", true),
        ("Bearer YOUR_API_KEY", true),
        ("use a bearer token", true),
        (std::str::from_utf8(SECRET).unwrap(), false),
        ("fixture-private-header-value", false),
        ("fixture-unknown-header-value", false),
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
    for (echoed, public) in metadata_header_echoes() {
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
        if public {
            let completion = result.unwrap();
            assert!(matches!(
                completion.terminal,
                ProviderGatewayTerminal::Completed { .. }
            ));
            assert!(
                completion
                    .frames
                    .iter()
                    .any(|frame| frame.payload_json().contains(echoed))
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
fn responses_header_stream_preserves_fragmented_documentation_and_blocks_credentials() {
    for (source, split, public) in [
        (
            "Bearer process.env.OPENAI_API_KEY",
            "Bearer process.env.OPENAI".len(),
            true,
        ),
        (
            "Bearer import.meta.env.PUBLIC_API_TOKEN",
            "Bearer import.meta.env.".len(),
            true,
        ),
        (
            "Bearer YOURAPPLICATIONTOKEN_PLACEHOLDER",
            "Bearer YOURAPPLICATIONTOKEN".len(),
            true,
        ),
        ("use a bearer token", "use a bea".len(), true),
        (
            "https://public.example/app",
            "https://public.example".len(),
            true,
        ),
        (
            std::str::from_utf8(SECRET).unwrap(),
            SECRET.len() / 2,
            false,
        ),
        (
            "fixture-private-header-value",
            "fixture-private".len(),
            false,
        ),
    ] {
        let mut events = responses_api_body(source)
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        let delta_index = events
            .iter()
            .position(|event| event["type"] == "response.custom_tool_call_input.delta")
            .unwrap();
        let mut tail = events[delta_index].clone();
        events[delta_index]["delta"] = serde_json::json!(&source[..split]);
        tail["delta"] = serde_json::json!(&source[split..]);
        events.insert(delta_index + 1, tail);
        let body = responses_wire(&events);
        let fixture = TlsFixture::start(vec![TestResponse {
            status: "200 OK",
            content_type: "text/event-stream",
            body: &body,
            declared_length: None,
            delay: Duration::ZERO,
        }]);
        let adapter = HttpsSseProviderAdapter::try_new(
            metadata_header_settings(&fixture)
                .with_openai_responses(32_768)
                .unwrap(),
        )
        .unwrap();
        let exchange = ModelExchangeId("mdl_00000000000000000000000001".into());
        let request_id = RequestId("req_00000000000000000000000001".into());
        let payload = responses_api_payload();
        let mut request = invocation(&exchange, &request_id);
        request.payload = &payload;
        adapter.open_https(request, SECRET).unwrap();
        let result = adapter.drain_canonical(&responses_api_receipt(exchange, request_id));
        assert_eq!(fixture.finish().len(), 1);
        if public {
            let completion = result.unwrap();
            let frames = completion
                .frames
                .iter()
                .map(|frame| {
                    serde_json::from_str::<serde_json::Value>(frame.payload_json()).unwrap()
                })
                .collect::<Vec<_>>();
            let deltas = frames
                .iter()
                .filter(|frame| frame["type"] == "tool_call_input_delta")
                .map(|frame| frame["delta"].as_str().unwrap())
                .collect::<Vec<_>>();
            assert_eq!(deltas, [&source[..split], &source[split..]]);
            assert!(frames.iter().any(|frame| {
                frame["type"] == "output_item_done" && frame["item"]["input"] == source
            }));
            assert!(matches!(
                completion.terminal,
                ProviderGatewayTerminal::Completed { .. }
            ));
        } else {
            assert_eq!(
                result.unwrap_err().kind(),
                HttpsSseProviderErrorKind::CredentialLeak
            );
        }
    }
}

#[test]
fn metadata_header_response_ids_and_models_preserve_public_text_and_block_credentials() {
    for field in ["id", "model"] {
        for (echoed, public) in metadata_header_echoes() {
            let mut chunk = serde_json::json!({
                "id":"header-response", "model":"fixture-model",
                "choices":[{"index":0, "delta":{"content":"public fixture output"}, "finish_reason":"stop"}],
                "usage":{"prompt_tokens":2, "completion_tokens":1}
            });
            chunk[field] = serde_json::json!(echoed);
            let body = format!("data: {chunk}\n\ndata: [DONE]\n\n");
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
            if public {
                let completion = result.unwrap();
                assert!(matches!(
                    completion.terminal,
                    ProviderGatewayTerminal::Completed { .. }
                ));
                if field == "model" {
                    assert!(completion.frames.iter().any(|frame| {
                        let value: serde_json::Value =
                            serde_json::from_str(frame.payload_json()).unwrap();
                        value["type"] == "server_model" && value["model"] == echoed
                    }));
                }
            } else {
                let error = result.unwrap_err();
                assert_eq!(error.kind(), HttpsSseProviderErrorKind::CredentialLeak);
                assert!(!format!("{error:?}").contains(echoed));
            }
        }
    }
}

fn http_referer_values() -> [(&'static str, bool); 12] {
    [
        ("https://public.example/app", true),
        ("http://public.example/app", true),
        ("http://localhost/app", true),
        ("http://192.168.1.2/app", true),
        ("https://public.example/my%20app", true),
        (
            "https://public.example/app?api_key=fixture-query-value",
            false,
        ),
        (
            "https://fixture-user:fixture-password@public.example/app",
            false,
        ),
        ("https://public.example/app#fixture-fragment-value", false),
        ("ftp://public.example/app", false),
        ("public.example/app", false),
        (
            "https://public.example/sk-0123456789abcdefghijklmnop",
            false,
        ),
        (
            "https://public.example/%73%6b-0123456789abcdefghijklmnop",
            false,
        ),
    ]
}

#[test]
fn http_referer_stream_preserves_application_urls_and_blocks_credential_urls() {
    for (referer, public) in http_referer_values() {
        let body = metadata_header_sse(referer);
        let fixture = TlsFixture::start(vec![TestResponse {
            status: "200 OK",
            content_type: "text/event-stream",
            body: &body,
            declared_length: None,
            delay: Duration::ZERO,
        }]);
        let settings = config(&fixture)
            .with_openai_chat_completions(8192, ProviderTokenPricing::default())
            .unwrap()
            .with_custom_headers([("hTtP-rEfErEr".into(), referer.into())])
            .unwrap();
        let adapter = HttpsSseProviderAdapter::try_new(settings).unwrap();
        let exchange = ModelExchangeId("mdl_00000000000000000000000001".into());
        let request_id = RequestId("req_00000000000000000000000001".into());
        let payload = metadata_header_payload();
        let mut request = invocation(&exchange, &request_id);
        request.payload = &payload;
        adapter.open_https(request, SECRET).unwrap();
        let result = adapter.drain_canonical(&responses_api_receipt(exchange, request_id));
        let requests = fixture.finish();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            identity_header_values(&requests[0], "http-referer"),
            [referer]
        );
        if public {
            let completion = result.unwrap();
            assert!(completion.frames.iter().any(|frame| {
                let value: serde_json::Value = serde_json::from_str(frame.payload_json()).unwrap();
                value["type"] == "output_item_done"
                    && value["item"]["content"][0]["text"] == referer
            }));
        } else {
            assert_eq!(
                result.unwrap_err().kind(),
                HttpsSseProviderErrorKind::CredentialLeak
            );
        }
    }
}

#[test]
fn http_referer_error_metadata_preserves_application_urls_and_blocks_credentials() {
    for (referer, public) in http_referer_values() {
        let custom_headers = vec![
            ("hTtP-rEfErEr".to_owned(), referer.to_owned()),
            (
                "x-private-provider-header".to_owned(),
                "fixture-private-header-value".to_owned(),
            ),
            (
                "x-unknown-provider-header".to_owned(),
                "fixture-unknown-header-value".to_owned(),
            ),
        ];
        for echoed in [
            referer,
            std::str::from_utf8(SECRET).unwrap(),
            "fixture-private-header-value",
            "fixture-unknown-header-value",
        ] {
            let headers = [("x-request-id".parse().unwrap(), echoed.parse().unwrap())]
                .into_iter()
                .collect();
            let metadata = response_metadata(&headers, 400, SECRET, &custom_headers);
            assert_eq!(metadata.status, Some(400));
            assert_eq!(
                metadata.provider_request_id.as_deref(),
                (public && echoed == referer).then_some(echoed)
            );
        }
    }
}

#[test]
fn http_referer_http_error_log_preserves_application_urls_and_blocks_credential_urls() {
    for (referer, public) in http_referer_values() {
        let fixture = TlsFixture::start(vec![TestResponse {
            status: "400 Bad Request",
            content_type: "text/plain",
            body: referer,
            declared_length: None,
            delay: Duration::ZERO,
        }]);
        let directory = HttpErrorLogDirectory::new();
        let settings = config(&fixture)
            .with_custom_headers([("hTtP-rEfErEr".into(), referer.into())])
            .unwrap();
        let failure = fixture_http_failure(&fixture, &directory, settings);
        assert_eq!(fixture.finish().len(), 1);
        assert_eq!(failure.http_status, Some(400));
        assert!(!failure.retryable());
        let (metadata, retained) = retained_http_error(&directory, failure);
        assert_eq!(metadata["bodyRetained"], public);
        assert_eq!(metadata["responseBytesReceived"], referer.len());
        assert_eq!(retained, if public { referer.as_bytes() } else { &[] });
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

    for (echoed, public) in metadata_header_echoes() {
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
        for (credential, _) in metadata_header_echoes()
            .into_iter()
            .filter(|(_, public)| !public)
        {
            assert!(
                !output.contains(credential),
                "provider credential reached model output"
            );
        }
        if public {
            assert!(chunks.iter().all(|chunk| chunk.error.is_none()));
            assert!(output.contains(echoed));
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
    for (echoed, public) in metadata_header_echoes() {
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
        assert_eq!(metadata["bodyRetained"], public);
        assert_eq!(metadata["responseBytesReceived"], echoed.len());
        if public {
            assert_eq!(retained, echoed.as_bytes());
        } else {
            assert!(retained.is_empty());
        }
    }
}
