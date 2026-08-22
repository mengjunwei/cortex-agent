use super::*;

mod penalty_forward_tests {
    use super::*;
    use adk_rust::GenerateContentConfig;

    fn make_request(config: Option<GenerateContentConfig>) -> LlmRequest {
        LlmRequest {
            model: "test-model".to_string(),
            contents: vec![Content {
                role: "user".to_string(),
                parts: vec![Part::Text {
                    text: "hi".to_string(),
                }],
            }],
            config,
            tools: HashMap::new(),
            previous_response_id: None,
        }
    }

    #[test]
    fn forwards_frequency_and_presence_penalty() {
        let req = make_request(Some(GenerateContentConfig {
            frequency_penalty: Some(0.4),
            presence_penalty: Some(0.3),
            ..Default::default()
        }));
        let re: Option<ReasoningEffort> = None;
        let json = build_request_json(
            "test-model",
            &req,
            &re,
            &GenericSchemaAdapter,
            &SCHEMA_CACHE,
        )
        .expect("build_request_json failed");
        let fp = json["frequency_penalty"]
            .as_f64()
            .expect("frequency_penalty should be a number");
        let pp = json["presence_penalty"]
            .as_f64()
            .expect("presence_penalty should be a number");
        assert!(
            (fp - 0.4).abs() < 1e-5,
            "frequency_penalty should be ~0.4, got {fp}"
        );
        assert!(
            (pp - 0.3).abs() < 1e-5,
            "presence_penalty should be ~0.3, got {pp}"
        );
    }

    #[test]
    fn omits_penalty_when_not_set() {
        let req = make_request(Some(GenerateContentConfig::default()));
        let re: Option<ReasoningEffort> = None;
        let json = build_request_json(
            "test-model",
            &req,
            &re,
            &GenericSchemaAdapter,
            &SCHEMA_CACHE,
        )
        .expect("build_request_json failed");
        assert!(
            json.get("frequency_penalty").is_none_or(|v| v.is_null()),
            "frequency_penalty should be absent when not set"
        );
    }
}

mod streaming_usage_tests {
    use super::*;
    use adk_rust::GenerateContentConfig;
    use futures::StreamExt;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn make_request() -> LlmRequest {
        LlmRequest {
            model: "test-model".to_string(),
            contents: vec![Content {
                role: "user".to_string(),
                parts: vec![Part::Text {
                    text: "hi".to_string(),
                }],
            }],
            config: Some(GenerateContentConfig::default()),
            tools: HashMap::new(),
            previous_response_id: None,
        }
    }

    async fn mount_sse(server: &MockServer, body: &'static str) {
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(body),
            )
            .mount(server)
            .await;
    }

    async fn collect_until_finish(mut stream: LlmResponseStream) -> LlmResponse {
        while let Some(chunk) = stream.next().await {
            let resp = chunk.expect("chunk error");
            if resp.finish_reason.is_some() || resp.turn_complete {
                return resp;
            }
        }
        panic!("stream ended without finish_reason/turn_complete frame");
    }

    #[tokio::test]
    async fn usage_chunk_after_finish_reason_is_attached_to_final_response() {
        let server = MockServer::start().await;
        mount_sse(
            &server,
            concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"he\"},\"index\":0}]}\n\n",
                "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\",\"index\":0}]}\n\n",
                "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":7,\"total_tokens\":18}}\n\n",
                "data: [DONE]\n\n",
            ),
        )
        .await;

        let llm = OpenAICustomCompatible::new(OpenAICustomCompatibleConfig::new(
            "k", "test-model", server.uri(),
        ));
        let stream = llm.generate_content(make_request(), true).await.expect("stream");
        let resp = collect_until_finish(stream).await;
        assert_eq!(resp.finish_reason, Some(FinishReason::Stop));
        assert!(resp.turn_complete);
        let u = resp.usage_metadata.expect("final frame should carry usage");
        assert_eq!(u.total_token_count, 18);
        assert_eq!(u.prompt_token_count, 11);
        assert_eq!(u.candidates_token_count, 7);
    }

    #[tokio::test]
    async fn usage_on_finish_chunk_itself_still_attached() {
        let server = MockServer::start().await;
        mount_sse(
            &server,
            concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"index\":0}]}\n\n",
                "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\",\"index\":0}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2,\"total_tokens\":5}}\n\n",
                "data: [DONE]\n\n",
            ),
        )
        .await;

        let llm = OpenAICustomCompatible::new(OpenAICustomCompatibleConfig::new(
            "k", "test-model", server.uri(),
        ));
        let stream = llm.generate_content(make_request(), true).await.expect("stream");
        let resp = collect_until_finish(stream).await;
        let u = resp.usage_metadata.expect("usage on finish chunk should be kept");
        assert_eq!(u.total_token_count, 5);
    }

    #[tokio::test]
    async fn tool_call_final_response_also_gets_trailing_usage() {
        let server = MockServer::start().await;
        mount_sse(
            &server,
            concat!(
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"get_time\",\"arguments\":\"\"}}]},\"index\":0}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{}\"}}]},\"index\":0}]}\n\n",
                "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\",\"index\":0}]}\n\n",
                "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":20,\"completion_tokens\":4,\"total_tokens\":24}}\n\n",
                "data: [DONE]\n\n",
            ),
        )
        .await;

        let llm = OpenAICustomCompatible::new(OpenAICustomCompatibleConfig::new(
            "k", "test-model", server.uri(),
        ));
        let stream = llm.generate_content(make_request(), true).await.expect("stream");
        let resp = collect_until_finish(stream).await;
        assert_eq!(resp.finish_reason, Some(FinishReason::Stop));
        let content = resp.content.expect("final FC frame should have content");
        assert!(
            content
                .parts
                .iter()
                .any(|p| matches!(p, Part::FunctionCall { name, .. } if name == "get_time")),
            "final frame should contain the accumulated tool call, got {:?}",
            content.parts
        );
        let u = resp.usage_metadata.expect("trailing usage should be attached");
        assert_eq!(u.total_token_count, 24);
    }

    #[tokio::test]
    async fn stream_without_usage_chunk_still_yields_finish_frame() {
        let server = MockServer::start().await;
        mount_sse(
            &server,
            concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"index\":0}]}\n\n",
                "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\",\"index\":0}]}\n\n",
                "data: [DONE]\n\n",
            ),
        )
        .await;

        let llm = OpenAICustomCompatible::new(OpenAICustomCompatibleConfig::new(
            "k", "test-model", server.uri(),
        ));
        let stream = llm.generate_content(make_request(), true).await.expect("stream");
        let resp = collect_until_finish(stream).await;
        assert_eq!(resp.finish_reason, Some(FinishReason::Stop));
        assert!(resp.turn_complete);
        assert!(resp.usage_metadata.is_none());
    }
}
