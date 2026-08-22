//! 协商层测试：探测/结论缓存、自愈降级、usage 口径归一。

use super::*;
// 仅测试使用的子模块项（生产路径不引用，避免非 test 构建的 unused import）
use super::probe::is_loopback_url;
use super::usage::RESPONSES_USAGE_CONVENTION;
use std::collections::HashMap;
use std::time::Duration;

use adk_rust::model::openai::OpenAIResponsesConfig;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::llm::openai::compat::OpenAICustomCompatibleConfig;

/// 进程级唯一后缀：RESPONSES_SUPPORT / RESPONSES_USAGE_CONVENTION 的 cache_key =
/// `base_url|model|SHA-256(api_key)`，所有测试 base_url 都用 wiremock 随机端口、
/// model/api_key 固定——一旦串行/高并发下 wiremock 端口被复用，key 即撞车，
/// ring/证据计数跨测试残留污染（usage 序列与 latch 判定错位，flaky 根因）。
/// 给 model 掺入全局原子序号，保证每个测试的 cache_key 真正唯一，与端口复用无关。
static TEST_MODEL_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn mk_auto(base_url: &str) -> OpenAiAutoLlm {
    let model = format!(
        "test-model-{}",
        TEST_MODEL_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let compat = OpenAICustomCompatible::new(OpenAICustomCompatibleConfig::new(
        "sk-test",
        &model,
        base_url,
    ));
    let responses = responses_client_for(base_url);
    OpenAiAutoLlm::new(compat, responses, base_url, "sk-test", &model)
}

// ── adapt_request_for_responses ──────────────────────────────────────

fn req_with_openai_ext(ext: serde_json::Value) -> LlmRequest {
    let mut r = mk_request();
    let mut config = adk_rust::GenerateContentConfig::default();
    config.extensions.insert("openai".to_string(), ext);
    r.config = Some(config);
    r
}

#[test]
fn adapt_converts_effort_to_nested_key() {
    let r = req_with_openai_ext(serde_json::json!({ "reasoning_effort": "high" }));
    let adapted = adapt_request_for_responses(r);
    let ext = adapted.config.unwrap().extensions["openai"].clone();
    assert_eq!(ext["reasoning"]["effort"], "high");
    // 原有的顶层键保留（无害，Responses 侧不读）
    assert_eq!(ext["reasoning_effort"], "high");
}

#[test]
fn adapt_skips_unsupported_levels() {
    // xhigh 不受 Responses 侧支持 → 不注入嵌套键（走模型默认）
    let r = req_with_openai_ext(serde_json::json!({ "reasoning_effort": "xhigh" }));
    let adapted = adapt_request_for_responses(r);
    let ext = adapted.config.unwrap().extensions["openai"].clone();
    assert!(ext.get("reasoning").is_none());
}

#[test]
fn adapt_noop_without_openai_ext() {
    let adapted = adapt_request_for_responses(mk_request());
    assert!(adapted.config.is_none());
}

// ── detect_responses_support ─────────────────────────────────────────

#[tokio::test]
async fn detect_2xx_means_supported() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "resp_1", "object": "response", "status": "completed", "output": []
        })))
        .mount(&server)
        .await;
    assert_eq!(
        detect_responses_support(&server.uri(), "sk-test", "test-model").await,
        ProbeVerdict::Supported
    );
}

#[tokio::test]
async fn detect_catchall_2xx_html_is_unsupported() {
    // catch-all 网关对任意 POST 都 200 返回 HTML：状态码不可信，
    // 必须校验响应体是 Responses 对象，否则误判后运行时 parse error
    // （无 upstream status）连自愈降级都触发不了。
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>welcome</html>"))
        .mount(&server)
        .await;
    assert_eq!(
        detect_responses_support(&server.uri(), "sk-test", "test-model").await,
        ProbeVerdict::Unsupported
    );
}

#[tokio::test]
async fn detect_404_means_unsupported() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
        .mount(&server)
        .await;
    assert_eq!(
        detect_responses_support(&server.uri(), "sk-test", "test-model").await,
        ProbeVerdict::Unsupported
    );
}

#[tokio::test]
async fn detect_401_means_unsupported() {
    // 鉴权失败（该 key 无 /responses 权限）：稳定状态，可缓存为不支持。
    // 回落 compat 后由其报出可读的鉴权错误。
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
        .mount(&server)
        .await;
    assert_eq!(
        detect_responses_support(&server.uri(), "sk-test", "test-model").await,
        ProbeVerdict::Unsupported
    );
}

#[tokio::test]
async fn detect_rate_limit_and_5xx_are_transient() {
    // 429/5xx 是瞬时状态：本次回落 compat 但只写短 TTL 负缓存，否则网关一次
    // 抖动就把可用的 /responses 长期打成 compat
    for status in [429u16, 500, 502, 503] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(ResponseTemplate::new(status).set_body_string("try later"))
            .mount(&server)
            .await;
        assert_eq!(
            detect_responses_support(&server.uri(), "sk-test", "test-model").await,
            ProbeVerdict::Transient,
            "HTTP {status} 应判为 Transient"
        );
    }
}

#[tokio::test]
async fn detect_network_error_is_transient() {
    // 端口 1 无监听、必然连接拒绝：结论不可靠 → Transient（写短 TTL 负缓存，窗口后重探）。
    // 不用 drop(MockServer)：并行测试可能复用其端口，收到真实响应污染断言。
    assert_eq!(
        detect_responses_support("http://127.0.0.1:1", "sk-test", "test-model").await,
        ProbeVerdict::Transient
    );
}

#[test]
fn cache_key_includes_api_key() {
    // 多用户同网关不同 key 的路由 ACL 可能不同：结论必须按 key 隔离，
    // 否则 A key 的「支持」结论会让无权限的 B key 永久打挂（401 不自愈）
    let base = "http://gw.example.com/v1";
    let a = OpenAiAutoLlm::new(
        OpenAICustomCompatible::new(OpenAICustomCompatibleConfig::new("key-a", "m", base)),
        responses_client_for(base),
        base,
        "key-a",
        "m",
    );
    let b = OpenAiAutoLlm::new(
        OpenAICustomCompatible::new(OpenAICustomCompatibleConfig::new("key-b", "m", base)),
        responses_client_for(base),
        base,
        "key-b",
        "m",
    );
    assert_ne!(a.cache_key, b.cache_key);
}

#[test]
fn cache_key_holds_no_plaintext_api_key() {
    // 缓存 key 是进程级全局 static，明文 api_key 不得驻留（对齐 DB 侧
    // AES 加密基线）——只允许 SHA-256 hex
    let base = "http://gw.example.com/v1";
    let auto = OpenAiAutoLlm::new(
        OpenAICustomCompatible::new(OpenAICustomCompatibleConfig::new(
            "sk-plaintext-secret",
            "m",
            base,
        )),
        responses_client_for(base),
        base,
        "sk-plaintext-secret",
        "m",
    );
    assert!(!auto.cache_key.contains("sk-plaintext-secret"));
}

#[test]
fn loopback_covers_full_v4_range_and_ipv6() {
    // 127.0.0.0/8 整段 + IPv6 loopback；host_str 对 IPv6 恒带方括号，
    // 用 Url::host() 匹配才可靠
    assert!(is_loopback_url("http://127.0.0.1:11434/v1"));
    assert!(is_loopback_url("http://127.0.0.2:11434/v1"));
    assert!(is_loopback_url("http://[::1]:11434/v1"));
    assert!(is_loopback_url("http://localhost:11434/v1"));
    assert!(!is_loopback_url("http://192.168.1.10:11434/v1"));
    assert!(!is_loopback_url("http://myhost.example.com:11434/v1"));
}

#[test]
fn parse_error_triggers_downgrade_check() {
    // 非 JSON 错误体（nginx 纯文本 404）经 async-openai 转为 JSONDeserialize，
    // 无 upstream status——parse 错误码是唯一可判信号，必须触发降级
    let parse_err = AdkError::new(
        adk_rust::ErrorComponent::Model,
        adk_rust::ErrorCategory::Internal,
        "model.openai_responses.parse",
        "parse error",
    );
    assert!(is_unsupported_responses_error(&parse_err));
    // 其他 Internal 错误不降级
    let other_err = AdkError::new(
        adk_rust::ErrorComponent::Model,
        adk_rust::ErrorCategory::Internal,
        "model.openai_responses.unknown",
        "unknown",
    );
    assert!(!is_unsupported_responses_error(&other_err));
}

#[tokio::test]
async fn schema_request_bypasses_responses_path() {
    // 带 response_schema 的请求必须直走 compat：上游不读 response_schema，
    // 走 responses 路径会让结构化输出静默失效
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "resp_1", "object": "response", "status": "completed", "output": []
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": "hello"},
                         "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        })))
        .mount(&server)
        .await;

    let auto = mk_auto(&server.uri());
    let mut request = mk_request();
    request.config = Some(adk_rust::GenerateContentConfig {
        response_schema: Some(serde_json::json!({"type": "object"})),
        ..Default::default()
    });

    let mut stream = auto.generate_content(request, false).await.expect("应成功");
    use futures::StreamExt;
    while let Some(chunk) = stream.next().await {
        chunk.expect("chunk 应成功");
    }
    // schema 防护在协商之前生效：连探测都不该发（0 次 /responses），
    // 真实调用必须走 /chat/completions
    let responses_calls = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path().ends_with("/responses"))
        .count();
    let chat_calls = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path().ends_with("/chat/completions"))
        .count();
    assert_eq!(
        responses_calls, 0,
        "schema 请求应先于探测直走 compat，不发任何 /responses 请求"
    );
    assert_eq!(chat_calls, 1, "schema 请求应走 compat 路径");
}

#[tokio::test]
async fn plain_text_404_self_heals_via_parse_error() {
    // nginx/one-api 纯文本 404：async-openai 无法解析错误体 → JSONDeserialize
    // （无 upstream status）→ 上游归为 parse 错误码 → 必须仍能触发自愈降级
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(404).set_body_string("<html>404 Not Found</html>"))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": "hello"},
                         "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        })))
        .mount(&server)
        .await;

    let auto = mk_auto(&server.uri());
    // 预置「支持」缓存（探测时返回的是合法 JSON——模拟路由漂移前状态）
    RESPONSES_SUPPORT
        .lock()
        .await
        .insert(auto.cache_key.clone(), ProbeCacheEntry::Supported);

    let mut stream = auto
        .generate_content(mk_request(), false)
        .await
        .expect("降级后应成功");
    use futures::StreamExt;
    let mut saw_text = false;
    while let Some(chunk) = stream.next().await {
        let resp = chunk.expect("chunk 应成功");
        if let Some(content) = resp.content
            && content
                .parts
                .iter()
                .any(|p| matches!(p, adk_rust::Part::Text { .. }))
        {
            saw_text = true;
        }
    }
    assert!(saw_text, "纯文本 404 应经 parse 错误码触发降级并成功");
}

fn responses_client_for(base_url: &str) -> OpenAIResponsesClient {
    OpenAIResponsesClient::new(
        OpenAIResponsesConfig::new("sk-test", "test-model")
            .with_base_url(base_url)
            .with_open_responses_mode(true),
    )
    .expect("responses client 构建失败")
}

// ── 协商 + 降级集成（wiremock 模拟端点） ────────────────────────────

fn mk_request() -> LlmRequest {
    use adk_rust::{Content, Part};
    LlmRequest {
        model: "test-model".to_string(),
        contents: vec![Content {
            role: "user".to_string(),
            parts: vec![Part::Text {
                text: "hi".to_string(),
            }],
        }],
        config: None,
        tools: HashMap::new(),
        previous_response_id: None,
    }
}

// 注意：两个集成测试不调用 clear_support_cache——key 含 wiremock 随机端口
// 天然唯一，而并行 clear 会互相清掉对方刚写入的断言状态（flaky）。

#[tokio::test]
async fn unsupported_endpoint_falls_back_to_chat_completions() {
    let server = MockServer::start().await;
    // /responses 404 → 探测判定不支持；实际调用走 /chat/completions
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": "hello"},
                         "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        })))
        .mount(&server)
        .await;

    let auto = mk_auto(&server.uri());
    let mut stream = auto
        .generate_content(mk_request(), false)
        .await
        .expect("应成功");
    use futures::StreamExt;
    let mut saw_text = false;
    while let Some(chunk) = stream.next().await {
        let resp = chunk.expect("chunk 应成功");
        if let Some(content) = resp.content
            && content
                .parts
                .iter()
                .any(|p| matches!(p, adk_rust::Part::Text { .. }))
        {
            saw_text = true;
        }
    }
    assert!(saw_text, "应从 compat 路径收到文本");
    // 缓存应记为不支持
    let cache = RESPONSES_SUPPORT.lock().await;
    assert_eq!(
        cache.get(&auto.cache_key),
        Some(&ProbeCacheEntry::Unsupported)
    );
}

#[tokio::test]
async fn auth_failure_on_responses_downgrades_to_compat() {
    // 空 key 端点：探测无鉴权头 200 判支持，运行时上游发占位 bearer 被网关 401
    // → 401 在自愈集合内，降级 compat 重发成功
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
            "error": {"message": "invalid token", "type": "invalid_request_error"}
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": "hello"},
                         "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        })))
        .mount(&server)
        .await;

    let auto = mk_auto(&server.uri());
    RESPONSES_SUPPORT
        .lock()
        .await
        .insert(auto.cache_key.clone(), ProbeCacheEntry::Supported); // 模拟探测已判支持

    let mut stream = auto
        .generate_content(mk_request(), false)
        .await
        .expect("降级后应成功");
    use futures::StreamExt;
    let mut saw_text = false;
    while let Some(chunk) = stream.next().await {
        let resp = chunk.expect("chunk 应成功");
        if let Some(content) = resp.content
            && content
                .parts
                .iter()
                .any(|p| matches!(p, adk_rust::Part::Text { .. }))
        {
            saw_text = true;
        }
    }
    assert!(saw_text, "401 降级后应从 compat 路径收到文本");
    // 401（带 status）→ 短冷却否定；比较 Instant 精度有抖动，断言三态 +
    // 冷却落在 [now, now+RETRY_PROBE_AFTER+ε] 区间
    let entry = RESPONSES_SUPPORT
        .lock()
        .await
        .get(&auto.cache_key)
        .copied()
        .expect("缓存应存在");
    match entry {
        ProbeCacheEntry::NegativeUntil(t) => {
            let now = Instant::now();
            assert!(
                t > now && now + RETRY_PROBE_AFTER + Duration::from_secs(5) > t,
                "401 降级应写入短冷却否定缓存: {entry:?}"
            );
        }
        other => panic!("401 降级应写入 NegativeUntil, 实际 {other:?}"),
    }
}

#[tokio::test]
async fn misjudged_endpoint_self_heals_to_compat() {
    let server = MockServer::start().await;
    // 场景：探测时 /responses 存在（200），实际调用时却 404（网关路由漂移）。
    // 错误体用 OpenAI 风格 JSON——async-openai 需要可解析的 error 结构才能
    // 把 HTTP 404 透传到 AdkError.upstream_status_code（真实网关均为此格式）。
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
            "error": {"message": "Not found", "type": "invalid_request_error"}
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": "hello"},
                         "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        })))
        .mount(&server)
        .await;

    let auto = mk_auto(&server.uri());
    // 预置误判缓存：本端点「支持」responses
    RESPONSES_SUPPORT
        .lock()
        .await
        .insert(auto.cache_key.clone(), ProbeCacheEntry::Supported);

    // 带 thinking_level 的请求：adapt 会给 responses 路径注入嵌套 reasoning 键，
    // 降级重发时必须用 adapt 之前的请求——否则嵌套键泄漏进 chat body（严格 API 400）
    let request = req_with_openai_ext(serde_json::json!({ "reasoning_effort": "high" }));
    let mut stream = auto
        .generate_content(request, false)
        .await
        .expect("降级后应成功");
    use futures::StreamExt;
    let mut saw_text = false;
    while let Some(chunk) = stream.next().await {
        let resp = chunk.expect("chunk 应成功");
        if let Some(content) = resp.content
            && content
                .parts
                .iter()
                .any(|p| matches!(p, adk_rust::Part::Text { .. }))
        {
            saw_text = true;
        }
    }
    assert!(saw_text, "自愈降级后应从 compat 路径收到文本");
    // 缓存被翻转为否定（带 status 的 404 → 短冷却 NegativeUntil，
    // 而非永久 Unsupported——探测 404 才写 Unsupported，运行时翻转是低置信否定）
    let cache = RESPONSES_SUPPORT.lock().await;
    // 404 带 status → 短冷却（RETRY_PROBE_AFTER），不是 parse 的长冷却
    match cache.get(&auto.cache_key) {
        Some(ProbeCacheEntry::NegativeUntil(t)) => {
            let now = Instant::now();
            assert!(
                *t > now && now + RETRY_PROBE_AFTER + Duration::from_secs(5) > *t,
                "404 自愈应为短冷却 NegativeUntil, 实际到期 {t:?}"
            );
        }
        other => panic!("404 自愈应翻转为 NegativeUntil, 实际 {other:?}"),
    }
    drop(cache);

    // 回归断言：降级发给 /chat/completions 的 body 不得含嵌套 reasoning 键
    // （顶层 reasoning_effort 合法，是 chat 路径自己的扩展键）
    let chat_body = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.url.path().contains("/chat/completions"))
        .expect("应收到 /chat/completions 请求")
        .body;
    let body_str = String::from_utf8_lossy(&chat_body);
    assert!(
        !body_str.contains("\"reasoning\""),
        "降级请求不应含嵌套 reasoning 键: {body_str}"
    );
    assert!(
        body_str.contains("reasoning_effort"),
        "chat 路径自身的 reasoning_effort 键应保留: {body_str}"
    );
}

// ── usage 口径归一（净输入检测 + gross 折算） ────────────────────────

/// 按请求次序返回预设 body 的响应器：同 matcher 多次挂载的匹配次序语义
/// 不可依赖，用共享计数器做确定性序列（越界重复最后一个 body，配合断言定位）
#[derive(Clone)]
struct SeqJsonResponder {
    bodies: std::sync::Arc<Vec<serde_json::Value>>,
    counter: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl wiremock::Respond for SeqJsonResponder {
    fn respond(&self, _request: &wiremock::Request) -> wiremock::ResponseTemplate {
        use std::sync::atomic::Ordering;
        let i = self.counter.fetch_add(1, Ordering::SeqCst);
        let body = self
            .bodies
            .get(i)
            .or_else(|| self.bodies.last())
            .expect("bodies 非空");
        wiremock::ResponseTemplate::new(200).set_body_json(body.clone())
    }
}

async fn mount_seq_responses(server: &MockServer, bodies: Vec<serde_json::Value>) {
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(SeqJsonResponder {
            bodies: std::sync::Arc::new(bodies),
            counter: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        })
        .mount(server)
        .await;
}

/// 合法 Response 对象（async-openai 全字段必填项齐备），usage 按参数构造
fn responses_body_with_usage(input: u32, cached: u32, output: u32) -> serde_json::Value {
    serde_json::json!({
        "id": "resp_1",
        "object": "response",
        "created_at": 0,
        "model": "test-model",
        "status": "completed",
        "output": [],
        "usage": {
            "input_tokens": input,
            "input_tokens_details": {"cached_tokens": cached},
            "output_tokens": output,
            "output_tokens_details": {"reasoning_tokens": 0},
            "total_tokens": input + output
        }
    })
}

/// tag 前缀 + n 条 user 消息的请求（同 tag 跨请求纯扩展可比；tag 区分无关对话）
fn request_with_tagged_items(tag: &str, n: usize) -> LlmRequest {
    use adk_rust::{Content, Part};
    assert!(n >= 1);
    LlmRequest {
        model: "test-model".to_string(),
        contents: (0..n)
            .map(|i| Content {
                role: "user".to_string(),
                parts: vec![Part::Text {
                    text: format!("{tag}-{i}"),
                }],
            })
            .collect(),
        config: None,
        tools: HashMap::new(),
        previous_response_id: None,
    }
}

/// n 条 user 消息的请求（msg-0..msg-{n-1}，跨请求纯扩展可比）
fn request_with_items(n: usize) -> LlmRequest {
    request_with_tagged_items("msg", n)
}

/// 首条即不同的对话（模拟并发/交错的无关会话：子 agent、query_understanding）
fn request_with_alt_items(n: usize) -> LlmRequest {
    request_with_tagged_items("other", n)
}

/// 预置「支持」缓存，跳过探测（探测会多消费一次 /responses mock 响应）
async fn seed_supported(auto: &OpenAiAutoLlm) {
    RESPONSES_SUPPORT
        .lock()
        .await
        .insert(auto.cache_key.clone(), ProbeCacheEntry::Supported);
}

/// 非流式调用并取末帧 usage（单帧即末帧）
async fn drain_final_usage(auto: &OpenAiAutoLlm, request: LlmRequest) -> adk_rust::UsageMetadata {
    let mut stream = auto.generate_content(request, false).await.expect("应成功");
    let mut last = None;
    while let Some(chunk) = stream.next().await {
        let resp = chunk.expect("chunk 应成功");
        if let Some(um) = resp.usage_metadata {
            last = Some(um);
        }
    }
    last.expect("末帧应带 usage")
}

fn net_input_latched(cache_key: &str) -> bool {
    RESPONSES_USAGE_CONVENTION
        .lock()
        .expect("usage 口径锁中毒")
        .get(cache_key)
        .map(|e| e.net_input)
        .unwrap_or(false)
}

#[test]
fn conv_fingerprint_prefix_semantics() {
    let fp1 = conv_fingerprint(&request_with_items(1).contents);
    let fp2 = conv_fingerprint(&request_with_items(2).contents);
    let fpb = conv_fingerprint(&request_with_alt_items(1).contents);
    assert_eq!(fp2.prefix_hash(1), Some(fp1.full_hash()), "纯扩展保留前缀哈希");
    assert_ne!(
        fp2.prefix_hash(1),
        Some(fpb.full_hash()),
        "不同对话的首条不同，前缀必不匹配"
    );
    assert_eq!(fp1.items(), 1);
    assert_eq!(fp2.items(), 2);
    assert_eq!(
        conv_fingerprint(&[]).prefix_hash(0),
        None,
        "空对话不参与判定"
    );
    assert_eq!(
        conv_fingerprint(&[]).prefix_hash(1),
        None,
        "越界前缀返回 None"
    );
}

#[tokio::test]
async fn gross_monotone_usage_passes_through_untouched() {
    // gross 口径（input 含 cache）对纯扩展对话单调不减 → 不判定、不折算，
    // usage 逐字节透传——诚实端点零行为变化是「不引入新 bug」的底线
    let server = MockServer::start().await;
    mount_seq_responses(
        &server,
        vec![
            responses_body_with_usage(1000, 800, 50), // 第 1 次调用
            responses_body_with_usage(1200, 900, 60), // 第 2 次调用
        ],
    )
    .await;

    let auto = mk_auto(&server.uri());
    seed_supported(&auto).await;

    let um1 = drain_final_usage(&auto, request_with_items(2)).await;
    assert_eq!(um1.prompt_token_count, 1000);
    assert_eq!(um1.total_token_count, 1050);
    assert_eq!(um1.cache_read_input_token_count, Some(800));

    let um2 = drain_final_usage(&auto, request_with_items(3)).await;
    assert_eq!(um2.prompt_token_count, 1200, "gross 端点不得折算 prompt");
    assert_eq!(um2.total_token_count, 1260, "gross 端点 total 保持 input+output");
    assert_eq!(um2.cache_read_input_token_count, Some(900));

    assert!(
        !net_input_latched(&auto.cache_key),
        "input 单调增长不得判定净口径"
    );
}

#[tokio::test]
async fn net_input_shrink_detected_and_folded_to_gross() {
    // 净口径端点：对话纯扩展而上报 input 收缩，且证据帧 cached > input
    // （净口径稳态：cache≈全前缀 > input≈本轮增量）。
    // **两次独立证据才 latch**（防网关漂移/重复帧误判）：
    // 第 1 次证据只观察不动作，第 2 次证据当帧判定并折算，此后无条件折算
    let server = MockServer::start().await;
    mount_seq_responses(
        &server,
        vec![
            responses_body_with_usage(5000, 0, 80),   // 第 1 次：无先验，透传
            responses_body_with_usage(4400, 4500, 40), // 第 2 次：证据#1（4400 ≤ 5000-512 且 4500 > 4400）
            responses_body_with_usage(3800, 3900, 30), // 第 3 次：证据#2（3800 ≤ 3888）→ latch+当帧折算
            responses_body_with_usage(200, 4000, 10),  // 第 4 次：已 latch，无条件折算
        ],
    )
    .await;

    let auto = mk_auto(&server.uri());
    seed_supported(&auto).await;

    let um1 = drain_final_usage(&auto, request_with_items(1)).await;
    assert_eq!(um1.prompt_token_count, 5000, "首次无先验，透传");
    assert_eq!(um1.total_token_count, 5080);

    // 证据#1：疑似净输入但不动作（单次证据可能是网关一次性漂移）
    let um2 = drain_final_usage(&auto, request_with_items(2)).await;
    assert_eq!(um2.prompt_token_count, 4400, "单次证据不得折算");
    assert_eq!(um2.total_token_count, 4440);
    assert!(!net_input_latched(&auto.cache_key), "单次证据不得 latch");

    // 证据#2：判定净口径 + 当帧折算
    let um3 = drain_final_usage(&auto, request_with_items(3)).await;
    assert_eq!(um3.prompt_token_count, 7700, "折算 prompt = 3800 + 3900");
    assert_eq!(um3.total_token_count, 7730, "折算 total = 3800 + 3900 + 30");
    assert_eq!(um3.cache_read_input_token_count, Some(3900), "cache_read 保留原值");
    assert!(net_input_latched(&auto.cache_key));

    // 第 4 次：无需再证，latch 直接折算（cached ≤ input 的帧也折——latch 是端点级）
    let um4 = drain_final_usage(&auto, request_with_items(4)).await;
    assert_eq!(um4.prompt_token_count, 4200, "latch 后无条件折算: 200 + 4000");
    assert_eq!(um4.total_token_count, 4210);
}

#[tokio::test]
async fn interleaved_main_conversation_shrink_still_detected() {
    // ring 的核心价值：QU/标题生成等无关会话与主会话逐轮交错（同端点同模型
    // → 同一缓存条目）时，主会话扩展请求与 ring 里的**主会话历史**比对前缀，
    // 仍能检出净口径——单条 baseline 下此场景检测永久失明（纯聊天会话）。
    // 两次证据也在交错下各自成立（证据#1、#2 来自不同的主会话请求对）
    let server = MockServer::start().await;
    // 调用次序：A(1条,1000) → B(1条,2000) → A扩展(2条,400) → B扩展(2条,2100)
    //          → A扩展(3条,300)
    mount_seq_responses(
        &server,
        vec![
            responses_body_with_usage(1000, 0, 80),    // convA
            responses_body_with_usage(2000, 0, 30),    // convB（打断 A 链）
            responses_body_with_usage(400, 900, 50),   // convA 扩展：证据#1（400 ≤ 1000-512）
            responses_body_with_usage(2100, 0, 20),    // convB 扩展（正常增长，无证据）
            responses_body_with_usage(300, 1000, 60),  // convA 再扩展：证据#2（300 ≤ 488）
        ],
    )
    .await;

    let auto = mk_auto(&server.uri());
    seed_supported(&auto).await;

    drain_final_usage(&auto, request_with_items(1)).await; // convA
    drain_final_usage(&auto, request_with_alt_items(1)).await; // convB（打断 A 链）
    // convA 扩展：虽与紧邻的 convB 前缀不匹配，但与 ring 里 convA 的记录
    // 前缀匹配 + input 1000→400 明显收缩 → 证据#1（单次不动作）
    let um3 = drain_final_usage(&auto, request_with_items(2)).await;
    assert_eq!(um3.prompt_token_count, 400, "证据#1 不得折算");
    assert!(!net_input_latched(&auto.cache_key), "证据#1 不得 latch");

    drain_final_usage(&auto, request_with_alt_items(2)).await; // convB 扩展
    // convA 再扩展：与 convA 历史（ring 内）前缀匹配 + 1000→300 → 证据#2 → latch+折算
    let um5 = drain_final_usage(&auto, request_with_items(3)).await;
    assert_eq!(um5.prompt_token_count, 1300, "交错下仍应检出: 300 + 1000");
    assert_eq!(um5.total_token_count, 1360);
    assert!(net_input_latched(&auto.cache_key));
}

#[tokio::test]
async fn foreign_shrink_without_prefix_match_never_latches() {
    // 安全性质（ring 不改变）：无关会话自身 input 小不得触发判定——它与会话
    // 历史**无前缀关系**（条数不多于任何 ring 记录 / 前缀哈希不匹配）。
    // 否则「大会话 50K → 小会话 2K」会被误判为净口径，对诚实端点错误折算
    // （双重计费式高估）。第三个无关会话把「前缀条件」钉进测试：若删掉
    // 前缀判定，两次无关收缩即凑满证据 latch，本测试变红
    let server = MockServer::start().await;
    // 调用次序：convA(2条,1000) → convB(1条,300,cached=900) → convC(1条,200,cached=800)
    mount_seq_responses(
        &server,
        vec![
            responses_body_with_usage(1000, 0, 80),  // convA
            responses_body_with_usage(300, 900, 50),  // convB（无关，形似收缩）
            responses_body_with_usage(200, 800, 20),  // convC（再一个无关会话）
        ],
    )
    .await;

    let auto = mk_auto(&server.uri());
    seed_supported(&auto).await;

    drain_final_usage(&auto, request_with_items(2)).await; // convA（2 条）
    // convB 只有 1 条，少于 convA 的 2 条 → 无前缀扩展关系，input 再小也不比
    let um2 = drain_final_usage(&auto, request_with_alt_items(1)).await;
    assert_eq!(um2.prompt_token_count, 300, "无前缀关系的 input 波动不得折算");
    assert_eq!(um2.total_token_count, 350);
    // convC 同理：与前两条会话均无前缀关系 → 两次「无关收缩」也不得凑满证据
    let um3 = drain_final_usage(&auto, request_with_tagged_items("third", 1)).await;
    assert_eq!(um3.prompt_token_count, 200, "前缀条件删除时此处应折算为 1000 而变红");
    assert!(
        !net_input_latched(&auto.cache_key),
        "无前缀关系的 input 波动不得 latch"
    );
}

#[tokio::test]
async fn regenerated_request_does_not_double_count_evidence() {
    // 同一指纹的请求（重发 / 用户 regenerate）命中同一收缩观察不得重复计证据
    // ——否则「故障转移漂移 + 重新生成」会用一条观察凑满两次证据误 latch。
    // 正常会话逐轮扩展指纹必然不同，守卫零误伤
    let server = MockServer::start().await;
    mount_seq_responses(
        &server,
        vec![
            responses_body_with_usage(1000, 0, 80),  // A：items=1
            responses_body_with_usage(400, 900, 50), // B：items=2，证据#1（400 ≤ 488 且 900 > 400）
            responses_body_with_usage(400, 900, 50), // B 重发（同指纹）：不得计证据#2
            responses_body_with_usage(300, 500, 10), // C：items=3，独立证据#2 → latch
        ],
    )
    .await;

    let auto = mk_auto(&server.uri());
    seed_supported(&auto).await;

    drain_final_usage(&auto, request_with_items(1)).await;
    drain_final_usage(&auto, request_with_items(2)).await; // 证据#1
    // regenerate：同指纹再发一次（对话形态不变）——证据不涨、不 latch
    let um3 = drain_final_usage(&auto, request_with_items(2)).await;
    assert_eq!(um3.prompt_token_count, 400, "同指纹重发不得折算");
    assert!(
        !net_input_latched(&auto.cache_key),
        "同指纹重发不得凑满两次证据"
    );
    // 新请求（指纹不同）携带独立证据（300 ≤ 488 且 500 > 300）→ latch + 折算
    let um4 = drain_final_usage(&auto, request_with_items(3)).await;
    assert_eq!(um4.prompt_token_count, 800, "独立证据应凑满并折算: 300 + 500");
    assert!(net_input_latched(&auto.cache_key), "独立证据应正常凑满");
}

#[tokio::test]
async fn within_margin_shrink_never_latches() {
    // margin 条件钉进测试：前缀 ✓ + cached>input ✓ 但收缩幅度在 margin 内
    // （5000→4950，margin=512）不得计证据。若删掉 margin 判定（退化为
    // input ≤ prev），两次小幅收缩会凑满证据 latch，本测试变红
    let server = MockServer::start().await;
    mount_seq_responses(
        &server,
        vec![
            responses_body_with_usage(5000, 0, 80),   // 基线（无先验透传）
            responses_body_with_usage(4950, 4990, 40), // 收缩 50 < margin 512
            responses_body_with_usage(4900, 4980, 30), // 再收缩 50，同样在 margin 内
        ],
    )
    .await;

    let auto = mk_auto(&server.uri());
    seed_supported(&auto).await;

    drain_final_usage(&auto, request_with_items(1)).await;
    drain_final_usage(&auto, request_with_items(2)).await;
    let um3 = drain_final_usage(&auto, request_with_items(3)).await;
    assert_eq!(um3.prompt_token_count, 4900, "margin 内收缩不得折算");
    assert_eq!(um3.total_token_count, 4930);
    assert!(
        !net_input_latched(&auto.cache_key),
        "margin 内收缩（tokenizer 零星抖动）不得 latch"
    );
}

#[tokio::test]
async fn ring_evicts_oldest_beyond_capacity() {
    // ring 容量驱逐钉进测试：连推 9 条后最老条目（t1）被逐出、最新条目（t9）
    // 保留——t9/t8 的扩展请求先后凑满两次证据 latch。若驱逐方向写反
    // （pop_back 逐最新）：t9 被逐、t9-扩展帧自身入环即被弹出，只剩 t8 一条
    // 可匹配证据，counter 永远到不了 2 → latch 断言变红
    let server = MockServer::start().await;
    // 9 个无关单条会话（cached=0 恒无证据），input 1100..=1900 递增
    let mut bodies: Vec<serde_json::Value> = (0..9)
        .map(|i| responses_body_with_usage(1100 + (i as u32) * 100, 0, 5))
        .collect();
    bodies.push(responses_body_with_usage(300, 4900, 50)); // t9 扩展：证据#1（匹配第 9 条）
    bodies.push(responses_body_with_usage(280, 4800, 40)); // t8 扩展：证据#2（匹配第 8 条）→ latch
    mount_seq_responses(&server, bodies).await;

    let auto = mk_auto(&server.uri());
    seed_supported(&auto).await;

    for i in 1..=9 {
        drain_final_usage(&auto, request_with_tagged_items(&format!("t{i}"), 1)).await;
    }
    // 满 8 容量后第 1 条（t1）已被逐出；t9（最新）保留
    let um10 = drain_final_usage(&auto, request_with_tagged_items("t9", 2)).await;
    assert_eq!(um10.prompt_token_count, 300, "证据#1 不折算");
    assert!(!net_input_latched(&auto.cache_key));
    // t8 仍在 ring 内（倒数第二新）→ 证据#2 → latch + 当帧折算
    let um11 = drain_final_usage(&auto, request_with_tagged_items("t8", 2)).await;
    assert_eq!(um11.prompt_token_count, 5080, "证据#2 折算: 280 + 4800");
    assert!(net_input_latched(&auto.cache_key), "驱逐方向写反时两次证据落空，此处变红");
}

#[tokio::test]
async fn zero_cache_shrink_never_latches() {
    // 零缓存帧（cached=0）永不构成证据：cached > input 恒假。gross 语义下
    // cached ⊆ input，该条件是防聚合网关误判的数学保险；对零缓存端点漏检
    // 无害——latch 后对 cached=0 帧折算也是 no-op
    let server = MockServer::start().await;
    mount_seq_responses(
        &server,
        vec![
            responses_body_with_usage(1000, 0, 80), // 第 1 次
            responses_body_with_usage(400, 0, 50),  // 第 2 次：形似收缩但 cached=0
            responses_body_with_usage(300, 0, 30),  // 第 3 次：同样 cached=0
        ],
    )
    .await;

    let auto = mk_auto(&server.uri());
    seed_supported(&auto).await;

    drain_final_usage(&auto, request_with_items(1)).await;
    drain_final_usage(&auto, request_with_items(2)).await;
    let um3 = drain_final_usage(&auto, request_with_items(3)).await;
    assert_eq!(um3.prompt_token_count, 300, "零缓存帧不得折算");
    assert_eq!(um3.total_token_count, 330);
    assert!(
        !net_input_latched(&auto.cache_key),
        "零缓存收缩不得 latch"
    );
}
