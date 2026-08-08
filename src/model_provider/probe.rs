//! 模型探测执行器层
//!
//! 按「能力标签」分流（chat/embedding/rerank），用轻量 reqwest 发最小请求验证
//! 模型存活。不复用 make_model_from_resolved（带 5 次重试退避，会拖垮 30s 超时）。
//! 探测专用解析 resolve_for_probe 绕过启用缓存与回退，能测禁用模型。

use crate::model_provider::enums::ProviderProtocol;

/// 探测分流类型（由模型 tags 决定）
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProbeKind {
    Chat,
    Embedding,
    Rerank,
}

/// 探测状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProbeStatus {
    Ok,
    Fail,
}

/// 单模型探测结果（编排产物，序列化为 JSON 给前端）
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProbeResult {
    pub model_id: String,
    pub model: String,
    pub provider_name: String,
    pub status: ProbeStatus,
    pub latency_ms: u64,
    pub probe_kind: ProbeKind,
    pub error: Option<String>,
    pub probed_at: String,
}

/// 探测专用解析结果（store::resolve_for_probe 产出，供执行器使用）
///
/// 与 ResolvedLlmConfig 的区别：不过滤启用状态、不回退；含 tags 与显示名。
#[derive(Debug, Clone)]
pub struct ResolvedForProbe {
    pub id: String,
    pub name: String,
    pub model: String,
    pub provider_name: String,
    pub vendor_name: String,
    pub base_url: String,
    pub api_key: String,
    pub protocol: ProviderProtocol,
    pub tags: Vec<String>,
}

use std::future::Future;
use std::pin::Pin;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use reqwest::header::{HeaderMap, HeaderValue};

/// 探测模块共享的 HTTP 客户端（带连接超时，不重试）
static HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(35))
        .build()
        .expect("探测 HTTP 客户端构建失败")
});

/// 执行器的最小产物（不含 model_id 等编排字段）
#[derive(Debug, Clone)]
struct ProbeOutcome {
    status: ProbeStatus,
    latency_ms: u64,
    error: Option<String>,
}

/// chat 探测（openai_compat → /chat/completions；anthropic → /v1/messages）
async fn probe_chat(resolved: &ResolvedForProbe) -> ProbeOutcome {
    probe_kind(resolved, ProbeKind::Chat, |r| {
        // clone 字段进 async move 块拥有所有权，避免「返回的 Future 借用闭包自身数据」
        let base_url = r.base_url.clone();
        let model = r.model.clone();
        let api_key = r.api_key.clone();
        let protocol = r.protocol;
        Box::pin(async move {
            match protocol {
                ProviderProtocol::OpenAiCompat => {
                    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
                    let body = serde_json::json!({
                        "model": model,
                        "messages": [{"role": "user", "content": "hi"}],
                        "max_tokens": 1,
                    });
                    send_openai_style(url, api_key, body).await
                }
                ProviderProtocol::Anthropic => send_anthropic_style(base_url, model, api_key).await,
            }
        })
    })
    .await
}

/// embedding 探测（仅 openai_compat；Anthropic 协议不支持，直接 Fail）
async fn probe_embedding(resolved: &ResolvedForProbe) -> ProbeOutcome {
    if resolved.protocol == ProviderProtocol::Anthropic {
        return ProbeOutcome {
            status: ProbeStatus::Fail,
            latency_ms: 0,
            error: Some("Anthropic 协议不支持 embedding 探测，请检查协议/标签配置".to_string()),
        };
    }
    probe_kind(resolved, ProbeKind::Embedding, |r| {
        // clone 字段进 async move 块拥有所有权，避免「返回的 Future 借用闭包自身数据」
        let base_url = r.base_url.clone();
        let model = r.model.clone();
        let api_key = r.api_key.clone();
        Box::pin(async move {
            let url = format!("{}/embeddings", base_url.trim_end_matches('/'));
            let body = serde_json::json!({ "model": model, "input": ["hi"] });
            send_openai_style(url, api_key, body).await
        })
    })
    .await
}

/// rerank 探测（仅 openai_compat；Anthropic 不支持，走与 embedding 相同的拒绝逻辑）
async fn probe_rerank(resolved: &ResolvedForProbe) -> ProbeOutcome {
    if resolved.protocol == ProviderProtocol::Anthropic {
        return ProbeOutcome {
            status: ProbeStatus::Fail,
            latency_ms: 0,
            error: Some("Anthropic 协议不支持 rerank 探测，请检查协议/标签配置".to_string()),
        };
    }
    probe_kind(resolved, ProbeKind::Rerank, |r| {
        // clone 字段进 async move 块拥有所有权，避免「返回的 Future 借用闭包自身数据」
        let base_url = r.base_url.clone();
        let model = r.model.clone();
        let api_key = r.api_key.clone();
        Box::pin(async move {
            let url = format!("{}/rerank", base_url.trim_end_matches('/'));
            let body = serde_json::json!({
                "model": model,
                "query": "a",
                "documents": ["b", "c"],
                "top_n": 1
            });
            send_openai_style(url, api_key, body).await
        })
    })
    .await
}

/// 通用执行骨架：发请求 → 计时 → 读响应分类 → 组装 ProbeOutcome。
///
/// `send` 闭包接收 `&ResolvedForProbe`（仅用于 clone 字段进异步块），返回发送 Future；
/// 返回类型为 `Pin<Box<impl Future>>`（每个闭包的 async move 块类型唯一，满足单态化）。
async fn probe_kind<F, Fut>(resolved: &ResolvedForProbe, kind: ProbeKind, send: F) -> ProbeOutcome
where
    F: FnOnce(&ResolvedForProbe) -> Fut,
    Fut: std::future::Future<Output = Result<reqwest::Response, String>>,
{
    let start = Instant::now();
    let (status, error) = match send(resolved).await {
        Ok(resp) => {
            let code = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            match classify_response(code, &body, kind) {
                None => (ProbeStatus::Ok, None),
                Some(err) => (ProbeStatus::Fail, Some(err)),
            }
        }
        Err(e) => (ProbeStatus::Fail, Some(e)),
    };
    ProbeOutcome {
        status,
        latency_ms: start.elapsed().as_millis() as u64,
        error,
    }
}

/// 探测单个已解析模型：按 tags 分流 → 执行 → 超时包裹 → 组装 ProbeResult。
///
/// `timeout` 参数化为可测（生产传 [`PROBE_TIMEOUT`]，测试传小值验证超时分支）。
pub async fn probe_one(resolved: &ResolvedForProbe, timeout: Duration) -> ProbeResult {
    let kind = classify_probe_kind(&resolved.tags);
    // 三类执行器各有独立的 future 类型，装箱为统一的 trait object 才能流入 timeout 包裹
    let exec: Pin<Box<dyn Future<Output = ProbeOutcome> + Send>> = match kind {
        ProbeKind::Chat => Box::pin(probe_chat(resolved)),
        ProbeKind::Embedding => Box::pin(probe_embedding(resolved)),
        ProbeKind::Rerank => Box::pin(probe_rerank(resolved)),
    };
    let outcome = match tokio::time::timeout(timeout, exec).await {
        Ok(o) => o,
        Err(_) => ProbeOutcome {
            status: ProbeStatus::Fail,
            latency_ms: timeout.as_millis() as u64,
            error: Some(format!(
                "探测超时（{}s），请检查 base_url 是否可达或模型是否响应过慢",
                timeout.as_secs()
            )),
        },
    };
    ProbeResult {
        model_id: resolved.id.clone(),
        model: resolved.model.clone(),
        provider_name: resolved.provider_name.clone(),
        status: outcome.status,
        latency_ms: outcome.latency_ms,
        probe_kind: kind,
        error: outcome.error,
        probed_at: chrono::Utc::now().to_rfc3339(),
    }
}

/// 生产环境探测超时（30s），供编排层使用
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// OpenAI 风格 POST（Bearer 鉴权）；api_key 为空不发鉴权头（兼容 Ollama 等本地端点）
async fn send_openai_style(
    url: String,
    api_key: String,
    body: serde_json::Value,
) -> Result<reqwest::Response, String> {
    let mut req = HTTP_CLIENT.post(&url).json(&body);
    if !api_key.is_empty() {
        req = req.bearer_auth(&api_key);
    }
    req.send()
        .await
        .map_err(|e| format!("无法连接到 base_url，请检查地址与网络。{e}"))
}

/// Anthropic 风格 POST（x-api-key + anthropic-version 鉴权）
async fn send_anthropic_style(
    base_url: String,
    model: String,
    api_key: String,
) -> Result<reqwest::Response, String> {
    let base = if base_url.trim().is_empty() {
        "https://api.anthropic.com"
    } else {
        base_url.trim_end_matches('/')
    };
    let url = format!("{base}/v1/messages");
    let body = serde_json::json!({
        "model": model,
        "max_tokens": 1,
        "messages": [{"role": "user", "content": "hi"}],
    });
    let mut headers = HeaderMap::new();
    if !api_key.is_empty() {
        headers.insert(
            "x-api-key",
            HeaderValue::from_str(&api_key).unwrap_or(HeaderValue::from_static("")),
        );
    }
    headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
    HTTP_CLIENT
        .post(&url)
        .headers(headers)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("无法连接到 base_url，请检查地址与网络。{e}"))
}

/// 按 tags 判定探测分流（确定性规则：对话优先，无标签兜底 chat）
///
/// - 含 "chat" → Chat
/// - 否则含 "embedding" → Embedding
/// - 否则含 "rerank" → Rerank
/// - 否则 → Chat
pub fn classify_probe_kind(tags: &[String]) -> ProbeKind {
    let has = |k: &str| tags.iter().any(|t| t.eq_ignore_ascii_case(k));
    if has("chat") {
        ProbeKind::Chat
    } else if has("embedding") {
        ProbeKind::Embedding
    } else if has("rerank") {
        ProbeKind::Rerank
    } else {
        ProbeKind::Chat
    }
}

/// 把 HTTP 响应映射为错误文案。成功（2xx）返回 None，失败返回 Some(可操作错误信息)。
///
/// 优先解析 OpenAI 风格 `{"error":{"message":...}}`，失败回退原始 body。
/// body 截断到 200 字符。rerank 失败追加通用格式提示。
pub fn classify_response(status: u16, body: &str, kind: ProbeKind) -> Option<String> {
    if (200..300).contains(&status) {
        return None;
    }
    let detail = extract_error_message(body);
    let detail = truncate(detail, 200);
    let msg = match status {
        401 | 403 => format!("鉴权失败（{status}），请检查 API Key。{detail}"),
        404 => format!("端点不存在（404），请检查 base_url / model 是否正确。{detail}"),
        429 => format!("触发限流（429），模型可能存活但被限速，稍后重试。{detail}"),
        s if (500..600).contains(&s) => format!("服务端错误（{s}），供应商异常。{detail}"),
        _ => format!("探测失败（HTTP {status}）。{detail}"),
    };
    if kind == ProbeKind::Rerank {
        Some(format!(
            "{msg}\n提示：rerank 采用通用 /rerank 格式，部分厂商接口不同可能导致误报，建议以实际业务调用为准。"
        ))
    } else {
        Some(msg)
    }
}

/// 从响应体提取错误信息（OpenAI 风格 `{"error":{"message":...}}`，否则回退原文）
fn extract_error_message(body: &str) -> String {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(msg) = v
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
        {
            return msg.to_string();
        }
        // 个别厂商用顶层 message
        if let Some(msg) = v.get("message").and_then(|m| m.as_str()) {
            return msg.to_string();
        }
    }
    body.to_string()
}

/// 截断字符串到 max 字符（按 char 边界）
fn truncate(s: String, max: usize) -> String {
    if s.chars().count() <= max {
        s
    } else {
        s.chars().take(max).collect::<String>() + "…"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn classify_chat_first() {
        // 含 chat 即走 chat（即使同时有其他修饰标签）
        assert_eq!(classify_probe_kind(&tags(&["chat"])), ProbeKind::Chat);
        assert_eq!(
            classify_probe_kind(&tags(&["chat", "reasoning"])),
            ProbeKind::Chat
        );
        assert_eq!(
            classify_probe_kind(&tags(&["chat", "vision", "tool_use"])),
            ProbeKind::Chat
        );
    }

    #[test]
    fn classify_embedding_when_no_chat() {
        assert_eq!(
            classify_probe_kind(&tags(&["embedding"])),
            ProbeKind::Embedding
        );
        // embedding + rerank 无 chat → embedding（embedding 优先）
        assert_eq!(
            classify_probe_kind(&tags(&["embedding", "rerank"])),
            ProbeKind::Embedding
        );
    }

    #[test]
    fn classify_rerank_when_alone() {
        assert_eq!(classify_probe_kind(&tags(&["rerank"])), ProbeKind::Rerank);
    }

    #[test]
    fn classify_defaults_to_chat() {
        // 无标签或仅修饰标签 → chat 兜底
        assert_eq!(classify_probe_kind(&tags(&[])), ProbeKind::Chat);
        assert_eq!(classify_probe_kind(&tags(&["vision"])), ProbeKind::Chat);
        assert_eq!(
            classify_probe_kind(&tags(&["reasoning", "tool_use"])),
            ProbeKind::Chat
        );
    }

    #[test]
    fn classify_case_insensitive() {
        // 大小写不敏感
        assert_eq!(classify_probe_kind(&tags(&["CHAT"])), ProbeKind::Chat);
        assert_eq!(
            classify_probe_kind(&tags(&["Embedding"])),
            ProbeKind::Embedding
        );
    }

    #[test]
    fn classify_ok_returns_none() {
        // 2xx 成功 → None（无错误）
        assert_eq!(classify_response(200, "", ProbeKind::Chat), None);
        assert_eq!(classify_response(204, "{}", ProbeKind::Embedding), None);
    }

    #[test]
    fn classify_auth_errors() {
        let e401 = classify_response(
            401,
            r#"{"error":{"message":"invalid api key"}}"#,
            ProbeKind::Chat,
        )
        .unwrap();
        assert!(e401.contains("鉴权失败"));
        assert!(e401.contains("401"));
        assert!(e401.contains("invalid api key"));

        let e403 = classify_response(403, "forbidden", ProbeKind::Chat).unwrap();
        assert!(e403.contains("鉴权失败"));
        assert!(e403.contains("403"));
    }

    #[test]
    fn classify_not_found() {
        let e = classify_response(404, "no such model", ProbeKind::Chat).unwrap();
        assert!(e.contains("端点不存在"));
        assert!(e.contains("404"));
        assert!(e.contains("no such model"));
    }

    #[test]
    fn classify_rate_limit() {
        let e = classify_response(429, "slow down", ProbeKind::Chat).unwrap();
        assert!(e.contains("限流"));
        assert!(e.contains("429"));
    }

    #[test]
    fn classify_server_error() {
        let e = classify_response(500, "boom", ProbeKind::Chat).unwrap();
        assert!(e.contains("服务端错误"));
        assert!(e.contains("500"));
        let e2 = classify_response(502, "bad gateway", ProbeKind::Chat).unwrap();
        assert!(e2.contains("502"));
    }

    #[test]
    fn classify_other_non_2xx() {
        let e = classify_response(418, "im a teapot", ProbeKind::Chat).unwrap();
        assert!(e.contains("418"));
        assert!(e.contains("im a teapot"));
    }

    #[test]
    fn classify_rerank_failure_has_hint() {
        // rerank 失败追加通用格式提示
        let e = classify_response(404, "not found", ProbeKind::Rerank).unwrap();
        assert!(e.contains("rerank"));
        assert!(e.contains("通用"));
    }

    #[test]
    fn classify_truncates_long_body() {
        // 超长 body 截断到 200 字符，避免错误文案爆炸
        let long = "x".repeat(1000);
        let e = classify_response(400, &long, ProbeKind::Chat).unwrap();
        assert!(e.len() < 600); // 文案前缀 + 截断 body 不会过长
    }

    #[test]
    fn probe_result_serializes_status_and_kind_lowercase() {
        // 前端按小写判断（status==='ok'/'fail'）并直接展示 probe_kind；
        // 锁定 #[serde(rename_all = "lowercase")] 契约，防 PascalCase 回归破坏前端。
        fn mk(status: ProbeStatus, kind: ProbeKind) -> ProbeResult {
            ProbeResult {
                model_id: "m1".into(),
                model: "test".into(),
                provider_name: "p".into(),
                status,
                latency_ms: 10,
                probe_kind: kind,
                error: None,
                probed_at: "2026-08-02T00:00:00Z".into(),
            }
        }
        let ok_chat = serde_json::to_string(&mk(ProbeStatus::Ok, ProbeKind::Chat)).unwrap();
        assert!(
            ok_chat.contains(r#""status":"ok""#),
            "Ok 应序列化为小写 ok: {ok_chat}"
        );
        assert!(
            ok_chat.contains(r#""probe_kind":"chat""#),
            "Chat 应序列化为小写 chat: {ok_chat}"
        );

        let fail_rerank = serde_json::to_string(&mk(ProbeStatus::Fail, ProbeKind::Rerank)).unwrap();
        assert!(
            fail_rerank.contains(r#""status":"fail""#),
            "Fail 应序列化为小写 fail: {fail_rerank}"
        );
        assert!(
            fail_rerank.contains(r#""probe_kind":"rerank""#),
            "Rerank 应序列化为小写 rerank: {fail_rerank}"
        );

        let embedding = serde_json::to_string(&mk(ProbeStatus::Ok, ProbeKind::Embedding)).unwrap();
        assert!(
            embedding.contains(r#""probe_kind":"embedding""#),
            "Embedding 应序列化为小写 embedding: {embedding}"
        );
    }

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn mk_resolved(base_url: &str, protocol: ProviderProtocol, tags: &[&str]) -> ResolvedForProbe {
        ResolvedForProbe {
            id: "m1".into(),
            name: "测试模型".into(),
            model: "test-model".into(),
            provider_name: "测试供应商".into(),
            vendor_name: "Vendor".into(),
            base_url: base_url.into(),
            api_key: "sk-test".into(),
            protocol,
            tags: tags.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[tokio::test]
    async fn probe_chat_openai_success() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content": "hi"}}]
            })))
            .mount(&server)
            .await;

        let r = mk_resolved(&server.uri(), ProviderProtocol::OpenAiCompat, &["chat"]);
        let out = probe_chat(&r).await;
        assert_eq!(out.status, ProbeStatus::Ok, "error={:?}", out.error);
        assert!(out.error.is_none());
    }

    #[tokio::test]
    async fn probe_chat_openai_auth_failure() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(401)
                    .set_body_string(r#"{"error":{"message":"invalid api key"}}"#),
            )
            .mount(&server)
            .await;

        let r = mk_resolved(&server.uri(), ProviderProtocol::OpenAiCompat, &["chat"]);
        let out = probe_chat(&r).await;
        assert_eq!(out.status, ProbeStatus::Fail);
        let err = out.error.unwrap();
        assert!(err.contains("鉴权失败"));
        assert!(err.contains("401"));
        assert!(err.contains("invalid api key"));
    }

    #[tokio::test]
    async fn probe_chat_anthropic_success() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content": [{"type": "text", "text": "hi"}]
            })))
            .mount(&server)
            .await;

        let r = mk_resolved(&server.uri(), ProviderProtocol::Anthropic, &["chat"]);
        let out = probe_chat(&r).await;
        assert_eq!(out.status, ProbeStatus::Ok, "error={:?}", out.error);
    }

    #[tokio::test]
    async fn probe_embedding_openai_success() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{"embedding": [0.1, 0.2]}]
            })))
            .mount(&server)
            .await;

        let r = mk_resolved(
            &server.uri(),
            ProviderProtocol::OpenAiCompat,
            &["embedding"],
        );
        let out = probe_embedding(&r).await;
        assert_eq!(out.status, ProbeStatus::Ok, "error={:?}", out.error);
    }

    #[tokio::test]
    async fn probe_embedding_anthropic_rejected_without_request() {
        // Anthropic 协议不支持 embedding：不发请求，直接 Fail
        let server = MockServer::start().await;
        // 故意不挂任何 mock —— 若发了请求，wiremock 会记录未匹配请求导致测试不稳
        let r = mk_resolved(&server.uri(), ProviderProtocol::Anthropic, &["embedding"]);
        let out = probe_embedding(&r).await;
        assert_eq!(out.status, ProbeStatus::Fail);
        let err = out.error.unwrap();
        assert!(err.contains("Anthropic"));
        assert!(err.contains("embedding"));
        // 确保未向 server 发请求
        assert_eq!(server.received_requests().await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn probe_rerank_openai_success() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rerank"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [{"index": 0, "relevance_score": 0.9}]
            })))
            .mount(&server)
            .await;

        let r = mk_resolved(&server.uri(), ProviderProtocol::OpenAiCompat, &["rerank"]);
        let out = probe_rerank(&r).await;
        assert_eq!(out.status, ProbeStatus::Ok, "error={:?}", out.error);
    }

    #[tokio::test]
    async fn probe_rerank_failure_has_compatibility_hint() {
        // rerank 失败应追加通用格式提示（覆盖 classify_response 的 rerank 分支真实命中）
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rerank"))
            .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;

        let r = mk_resolved(&server.uri(), ProviderProtocol::OpenAiCompat, &["rerank"]);
        let out = probe_rerank(&r).await;
        assert_eq!(out.status, ProbeStatus::Fail);
        let err = out.error.unwrap();
        assert!(err.contains("rerank"));
        assert!(err.contains("通用"));
    }

    #[tokio::test]
    async fn probe_one_dispatches_chat_by_tags() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"choices":[{}]})),
            )
            .mount(&server)
            .await;
        let r = mk_resolved(&server.uri(), ProviderProtocol::OpenAiCompat, &["chat"]);
        let res = probe_one(&r, Duration::from_secs(30)).await;
        assert_eq!(res.status, ProbeStatus::Ok);
        assert_eq!(res.probe_kind, ProbeKind::Chat);
        assert_eq!(res.model_id, "m1");
        assert_eq!(res.model, "test-model");
        assert!(!res.probed_at.is_empty());
    }

    #[tokio::test]
    async fn probe_one_dispatches_embedding_by_tags() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/embeddings"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"data":[{"embedding":[0.1]}]})),
            )
            .mount(&server)
            .await;
        let r = mk_resolved(
            &server.uri(),
            ProviderProtocol::OpenAiCompat,
            &["embedding"],
        );
        let res = probe_one(&r, Duration::from_secs(30)).await;
        assert_eq!(res.probe_kind, ProbeKind::Embedding);
        assert_eq!(res.status, ProbeStatus::Ok);
    }

    #[tokio::test]
    async fn probe_one_timeout_yields_fail() {
        // 服务端延迟响应；用极小 timeout 触发超时分支（不用真实 30s）
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"choices":[{}]}))
                    .set_delay(Duration::from_millis(500)),
            )
            .mount(&server)
            .await;
        let r = mk_resolved(&server.uri(), ProviderProtocol::OpenAiCompat, &["chat"]);
        let res = probe_one(&r, Duration::from_millis(50)).await;
        assert_eq!(res.status, ProbeStatus::Fail);
        assert!(res.error.as_deref().unwrap().contains("超时"));
    }
}
