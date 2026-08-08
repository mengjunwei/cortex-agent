# 模型供应商「模型探测」功能实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在模型供应商管理页新增「探测模型存活」能力——单模型/批量(可跨供应商)/全供应商三种粒度，按能力标签(chat/embedding/rerank)分流发最小请求，结果集中展示于独立面板。

**Architecture:** 后端新增探测专用解析 `resolve_for_probe`（绕过启用缓存与回退，能测禁用模型）+ 轻量 reqwest 执行器（不复用带 retry 的 LLM 客户端，避免重试拖垮 30s 超时）。GraphQL 单一 mutation `probeModels(input:{ids})`，全并发编排，单模型 30s 超时。前端勾选复选框(跨供应商) + 结果抽屉面板。

**Tech Stack:** Rust（diesel、async-graphql、reqwest 0.13、tokio、wiremock 0.6 测试）、Vue3 + Element Plus + Pinia。

## Global Constraints

- 后端验证命令：`cargo check --bin cortex-agent`（编译）、`cargo test`（单元测试）。工作目录 `D:/code/rust/cortex-agent`。
- 前端验证命令：`cd frontend && npm run dev`（手动验证）、`cd frontend && npm run build`（构建检查）。
- 全中文注释、UI 文案、错误提示；专业术语可保留英文。
- Git 工作流：直接在 main 提交，不切 feature 分支；每个任务结束 commit。
- 错误码用 `crate::server::response::code` 常量（`INVALID_PARAMS`=1001、`DATABASE`=3001）。
- GraphQL 单入口 `POST /api/graphql`，resolver 返回 `Json` 标量（内含 `{code,message,data}` 信封）；前端 `gql()` 拆信封得 `{data,code,message}`。
- 探测结果**不落库**（实时探测实时展示）。
- 探测不复用 `make_model_from_resolved`（带 5 次重试退避），用独立 reqwest 一次性请求。
- 探测解析 `resolve_for_probe` **不过滤启用状态、不回退**（探测核心诉求是测禁用/未启用模型）。

## File Structure

| 文件 | 职责 | 动作 |
|------|------|------|
| `src/model_provider/probe.rs`（新建） | 探测执行器层：数据结构、分流判定、错误分类、三类请求执行、超时包裹 | 新建 |
| `src/model_provider/mod.rs` | 声明 `pub mod probe;`、新增 `ResolvedForProbe`、re-export | 修改 |
| `src/model_provider/store/cache.rs` | 新增 `resolve_for_probe`（绕过缓存/回退的 DB 解析） | 修改 |
| `src/model_provider/dto.rs` | 新增 `ProbeModelsInput` | 修改 |
| `src/server/model_provider.rs` | 新增 `probe_models` 编排 + `probe_one` | 修改 |
| `src/server/graphql.rs` | MutationRoot 新增 `probe_models` | 修改 |
| `frontend/src/api/index.js` | 新增 `probeModels` | 修改 |
| `frontend/src/views/ModelProviderPage.vue` | 复选框列 + 探测徽标 + 工具栏按钮 + 结果抽屉 | 修改 |

---

## Task 1: probe.rs 基础结构与纯函数（分流判定 + 错误分类）

**Files:**
- Create: `src/model_provider/probe.rs`
- Modify: `src/model_provider/mod.rs`（声明模块 + ResolvedForProbe + re-export）

**Interfaces:**
- Consumes: `crate::model_provider::enums::ProviderProtocol`
- Produces: `ProbeKind`、`ProbeStatus`、`ProbeResult`、`ResolvedForProbe`、`classify_probe_kind(&[String]) -> ProbeKind`、`classify_response(u16, &str, ProbeKind) -> Option<String>`（成功返回 None，失败返回错误文案）

- [ ] **Step 1: 在 mod.rs 声明模块并新增 ResolvedForProbe**

修改 `src/model_provider/mod.rs`，把：

```rust
pub mod crypto;
pub mod dto;
pub mod enums;
pub mod store;

pub use store::ResolvedLlmConfig;
```

改为：

```rust
pub mod crypto;
pub mod dto;
pub mod enums;
pub mod probe;
pub mod store;

pub use probe::{ProbeKind, ProbeResult, ProbeStatus, ResolvedForProbe};
pub use store::ResolvedLlmConfig;
```

- [ ] **Step 2: 写 probe.rs 的失败测试（分流判定）**

创建 `src/model_provider/probe.rs`，先只写测试与桩：

```rust
//! 模型探测执行器层
//!
//! 按「能力标签」分流（chat/embedding/rerank），用轻量 reqwest 发最小请求验证
//! 模型存活。不复用 make_model_from_resolved（带 5 次重试退避，会拖垮 30s 超时）。
//! 探测专用解析 resolve_for_probe 绕过启用缓存与回退，能测禁用模型。

use crate::model_provider::enums::ProviderProtocol;

/// 探测分流类型（由模型 tags 决定）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeKind {
    Chat,
    Embedding,
    Rerank,
}

/// 探测状态
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

/// 按 tags 判定探测分流（确定性规则：对话优先，无标签兜底 chat）
///
/// - 含 "chat" → Chat
/// - 否则含 "embedding" → Embedding
/// - 否则含 "rerank" → Rerank
/// - 否则 → Chat
pub fn classify_probe_kind(tags: &[String]) -> ProbeKind {
    todo!()
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
}
```

- [ ] **Step 3: 运行测试确认失败**

Run: `cargo test --lib model_provider::probe::tests::classify 2>&1 | head -40`
Expected: panic（`classify_probe_kind` 是 `todo!()`）或编译失败提示。

- [ ] **Step 4: 实现 classify_probe_kind**

替换 `probe.rs` 中 `classify_probe_kind` 的 `todo!()`：

```rust
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
```

- [ ] **Step 5: 运行分流测试确认通过**

Run: `cargo test --lib model_provider::probe::tests::classify 2>&1 | tail -20`
Expected: 5 个 classify_* 测试全 PASS。

- [ ] **Step 6: 写错误分类纯函数的失败测试**

在 `probe.rs` 的 `tests` 模块追加（先不实现 `classify_response`）：

```rust
    #[test]
    fn classify_ok_returns_none() {
        // 2xx 成功 → None（无错误）
        assert_eq!(classify_response(200, "", ProbeKind::Chat), None);
        assert_eq!(classify_response(204, "{}", ProbeKind::Embedding), None);
    }

    #[test]
    fn classify_auth_errors() {
        let e401 = classify_response(401, r#"{"error":{"message":"invalid api key"}}"#, ProbeKind::Chat).unwrap();
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
```

- [ ] **Step 7: 运行测试确认失败**

Run: `cargo test --lib model_provider::probe::tests::classify_ok 2>&1 | tail -10`
Expected: 编译失败（`classify_response` 未定义）。

- [ ] **Step 8: 实现 classify_response**

在 `probe.rs`（`classify_probe_kind` 之后）追加：

```rust
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
        if let Some(msg) = v.get("error").and_then(|e| e.get("message")).and_then(|m| m.as_str())
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
```

- [ ] **Step 9: 运行全部 probe 测试确认通过**

Run: `cargo test --lib model_provider::probe 2>&1 | tail -25`
Expected: 全部 PASS（5 个 classify_* + 8 个 classify_response 相关）。

- [ ] **Step 10: 编译检查 + Commit**

Run: `cargo check --bin cortex-agent 2>&1 | tail -5`
Expected: 编译通过（probe 模块已声明、结构已定义）。

```bash
git add src/model_provider/probe.rs src/model_provider/mod.rs
git commit -m "$(cat <<'EOF'
feat(probe): 探测数据结构 + 分流判定 + HTTP 错误分类纯函数

classify_probe_kind 按 tags 分流(chat/embedding/rerank)，classify_response
把 HTTP 状态码映射为可操作错误文案。均为纯函数，单元测试覆盖判定矩阵。

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: chat 执行器（openai_compat + anthropic 双协议）

**Files:**
- Modify: `src/model_provider/probe.rs`

**Interfaces:**
- Consumes: `ResolvedForProbe`、`ProbeStatus`、`classify_response`、`classify_probe_kind`
- Produces: `probe_chat(&ResolvedForProbe) -> ProbeOutcome`（含 status/latency_ms/error），模块级 `HTTP_CLIENT`

- [ ] **Step 1: 定义 ProbeOutcome 与 HTTP_CLIENT，写 chat 成功测试**

在 `probe.rs`（结构体定义区之后、`classify_probe_kind` 之前）追加：

```rust
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
    todo!()
}
```

在 `tests` 模块追加（wiremock 桩）：

```rust
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
        Mock::given(method("POST").and(path("/chat/completions")))
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
        Mock::given(method("POST").and(path("/chat/completions")))
            .respond_with(ResponseTemplate::new(401).set_body_string(
                r#"{"error":{"message":"invalid api key"}}"#,
            ))
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
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test --lib model_provider::probe::tests::probe_chat 2>&1 | tail -15`
Expected: panic（`probe_chat` 是 `todo!()`）。

- [ ] **Step 3: 实现 probe_chat**

替换 `probe.rs` 中 `probe_chat` 的 `todo!()`：

```rust
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
                ProviderProtocol::Anthropic => {
                    send_anthropic_style(base_url, model, api_key).await
                }
            }
        })
    })
    .await
}

/// 通用执行骨架：发请求 → 计时 → 读响应分类 → 组装 ProbeOutcome。
///
/// `send` 闭包接收 `&ResolvedForProbe`（仅用于 clone 字段进异步块），返回发送 Future；
/// 返回类型为 `Pin<Box<impl Future>>`（每个闭包的 async move 块类型唯一，满足单态化）。
async fn probe_kind<F, Fut>(
    resolved: &ResolvedForProbe,
    kind: ProbeKind,
    send: F,
) -> ProbeOutcome
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
```

- [ ] **Step 4: 运行 chat 测试确认通过**

Run: `cargo test --lib model_provider::probe::tests::probe_chat 2>&1 | tail -15`
Expected: 2 个 probe_chat_* 测试 PASS（成功 + 401 鉴权失败带 body 文案）。

- [ ] **Step 5: 补 anthropic 成功测试**

在 `tests` 模块追加：

```rust
    #[tokio::test]
    async fn probe_chat_anthropic_success() {
        let server = MockServer::start().await;
        Mock::given(method("POST").and(path("/v1/messages")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content": [{"type": "text", "text": "hi"}]
            })))
            .mount(&server)
            .await;

        let r = mk_resolved(&server.uri(), ProviderProtocol::Anthropic, &["chat"]);
        let out = probe_chat(&r).await;
        assert_eq!(out.status, ProbeStatus::Ok, "error={:?}", out.error);
    }
```

Run: `cargo test --lib model_provider::probe::tests::probe_chat_anthropic 2>&1 | tail -10`
Expected: PASS。

- [ ] **Step 6: 编译检查 + Commit**

Run: `cargo check --bin cortex-agent 2>&1 | tail -5` → Expected: 通过。

```bash
git add src/model_provider/probe.rs
git commit -m "$(cat <<'EOF'
feat(probe): chat 执行器(openai_compat + anthropic 双协议)

probe_kind 通用骨架 + send_openai_style/send_anthropic_chat，wiremock
覆盖成功与 401 鉴权失败(body 文案精确分类)。

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: embedding + rerank 执行器 + 协议冲突

**Files:**
- Modify: `src/model_provider/probe.rs`

**Interfaces:**
- Consumes: `probe_kind`、`send_openai_style`、`ResolvedForProbe`、`ProviderProtocol`
- Produces: `probe_embedding(&ResolvedForProbe) -> ProbeOutcome`、`probe_rerank(&ResolvedForProbe) -> ProbeOutcome`

- [ ] **Step 1: 写 embedding 成功 + 协议冲突测试**

在 `tests` 模块追加：

```rust
    #[tokio::test]
    async fn probe_embedding_openai_success() {
        let server = MockServer::start().await;
        Mock::given(method("POST").and(path("/embeddings")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{"embedding": [0.1, 0.2]}]
            })))
            .mount(&server)
            .await;

        let r = mk_resolved(&server.uri(), ProviderProtocol::OpenAiCompat, &["embedding"]);
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
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test --lib model_provider::probe::tests::probe_embedding 2>&1 | tail -10`
Expected: 编译失败（`probe_embedding` 未定义）。

- [ ] **Step 3: 实现 probe_embedding**

在 `probe.rs`（`probe_chat` 之后）追加：

```rust
/// embedding 探测（仅 openai_compat；Anthropic 协议不支持，直接 Fail）
async fn probe_embedding(resolved: &ResolvedForProbe) -> ProbeOutcome {
    if resolved.protocol == ProviderProtocol::Anthropic {
        return ProbeOutcome {
            status: ProbeStatus::Fail,
            latency_ms: 0,
            error: Some(
                "Anthropic 协议不支持 embedding 探测，请检查协议/标签配置".to_string(),
            ),
        };
    }
    probe_kind(resolved, ProbeKind::Embedding, |r| {
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
```

- [ ] **Step 4: 运行 embedding 测试确认通过**

Run: `cargo test --lib model_provider::probe::tests::probe_embedding 2>&1 | tail -10`
Expected: 2 个 PASS（含 anthropic 拒绝且零请求验证）。

- [ ] **Step 5: 写 rerank 成功测试**

在 `tests` 模块追加：

```rust
    #[tokio::test]
    async fn probe_rerank_openai_success() {
        let server = MockServer::start().await;
        Mock::given(method("POST").and(path("/rerank")))
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
        Mock::given(method("POST").and(path("/rerank")))
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
```

- [ ] **Step 6: 实现 probe_rerank**

在 `probe.rs`（`probe_embedding` 之后）追加：

```rust
/// rerank 探测（仅 openai_compat；Anthropic 不支持，走与 embedding 相同的拒绝逻辑）
async fn probe_rerank(resolved: &ResolvedForProbe) -> ProbeOutcome {
    if resolved.protocol == ProviderProtocol::Anthropic {
        return ProbeOutcome {
            status: ProbeStatus::Fail,
            latency_ms: 0,
            error: Some(
                "Anthropic 协议不支持 rerank 探测，请检查协议/标签配置".to_string(),
            ),
        };
    }
    probe_kind(resolved, ProbeKind::Rerank, |r| {
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
```

- [ ] **Step 7: 运行 rerank 测试确认通过**

Run: `cargo test --lib model_provider::probe::tests::probe_rerank 2>&1 | tail -10`
Expected: 2 个 PASS。

- [ ] **Step 8: 全量 probe 测试 + 编译 + Commit**

Run: `cargo test --lib model_provider::probe 2>&1 | tail -20` → Expected: 全部 PASS。
Run: `cargo check --bin cortex-agent 2>&1 | tail -5` → Expected: 通过。

```bash
git add src/model_provider/probe.rs
git commit -m "$(cat <<'EOF'
feat(probe): embedding + rerank 执行器，协议冲突直接 Fail

embedding 走 /embeddings，rerank 走 /rerank(通用格式兜底)；Anthropic
协议下二者不发请求直接 Fail。wiremock 覆盖成功/失败/协议冲突/零请求。

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: 探测分流入口 + 超时包裹

**Files:**
- Modify: `src/model_provider/probe.rs`

**Interfaces:**
- Consumes: `classify_probe_kind`、`probe_chat`、`probe_embedding`、`probe_rerank`、`ResolvedForProbe`
- Produces: `pub async fn probe_one(resolved: &ResolvedForProbe, timeout: Duration) -> ProbeResult`

- [ ] **Step 1: 写分流 + 超时测试**

在 `tests` 模块追加：

```rust
    #[tokio::test]
    async fn probe_one_dispatches_chat_by_tags() {
        let server = MockServer::start().await;
        Mock::given(method("POST").and(path("/chat/completions")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"choices":[{}]})))
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
    async fn probe_one_timeout_yields_fail() {
        // 服务端延迟响应；用极小 timeout 触发超时分支
        let server = MockServer::start().await;
        Mock::given(method("POST").and(path("/chat/completions")))
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

    #[tokio::test]
    async fn probe_one_dispatches_embedding_by_tags() {
        let server = MockServer::start().await;
        Mock::given(method("POST").and(path("/embeddings")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"data":[{"embedding":[0.1]}]})))
            .mount(&server)
            .await;
        let r = mk_resolved(&server.uri(), ProviderProtocol::OpenAiCompat, &["embedding"]);
        let res = probe_one(&r, Duration::from_secs(30)).await;
        assert_eq!(res.probe_kind, ProbeKind::Embedding);
        assert_eq!(res.status, ProbeStatus::Ok);
    }
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test --lib model_provider::probe::tests::probe_one 2>&1 | tail -10`
Expected: 编译失败（`probe_one` 未定义）。

- [ ] **Step 3: 实现 probe_one**

在 `probe.rs`（执行器函数之后）追加：

```rust
/// 探测单个已解析模型：按 tags 分流 → 执行 → 30s 超时包裹 → 组装 ProbeResult。
///
/// `timeout` 参数化为可测（生产传 30s，测试传小值验证超时分支）。
pub async fn probe_one(resolved: &ResolvedForProbe, timeout: Duration) -> ProbeResult {
    let kind = classify_probe_kind(&resolved.tags);
    let exec = match kind {
        ProbeKind::Chat => probe_chat(resolved),
        ProbeKind::Embedding => probe_embedding(resolved),
        ProbeKind::Rerank => probe_rerank(resolved),
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

/// 生产环境探测超时（30s）
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
```

- [ ] **Step 4: 运行 probe_one 测试确认通过**

Run: `cargo test --lib model_provider::probe::tests::probe_one 2>&1 | tail -10`
Expected: 3 个 PASS（分流 chat、超时、分流 embedding）。

- [ ] **Step 5: 全量测试 + 编译 + Commit**

Run: `cargo test --lib model_provider::probe 2>&1 | tail -20` → Expected: 全部 PASS。
Run: `cargo check --bin cortex-agent 2>&1 | tail -5` → Expected: 通过。

```bash
git add src/model_provider/probe.rs
git commit -m "$(cat <<'EOF'
feat(probe): probe_one 分流入口 + 30s 超时包裹

按 classify_probe_kind 分流到三类执行器,tokio::time::timeout 包裹,超时
产 Fail + 超时文案;组装含 model_id/probed_at 的完整 ProbeResult。
PROBE_TIMEOUT=30s 常量供编排层用。

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: store resolve_for_probe（绕过缓存/回退的 DB 解析）

**Files:**
- Modify: `src/model_provider/store/cache.rs`
- Modify: `src/model_provider/mod.rs`（re-export `ResolvedForProbe`，Task 1 已声明 probe 模块，此处理确认 `pub use` 已含）

**Interfaces:**
- Consumes: `ModelProviderStore::{get_conn, codec}`、`ProviderRow`、`ProviderProtocol::parse`、`parse_tags`
- Produces: `ModelProviderStore::resolve_for_probe(&self, model_id: &str) -> Result<ResolvedForProbe, AppError>`

> **测试策略说明**：项目 store 层目前无 in-memory DB 单元测试设施，`resolve_for_probe` 强依赖真实 postgres。本任务以 **`cargo check` 保证类型/SQL 编译正确** + **手动验证**（Task 7 端到端时确认）为准，不写自动测试。

- [ ] **Step 1: 实现 resolve_for_probe**

在 `src/model_provider/store/cache.rs` 的 `impl ModelProviderStore { ... }` 块内（`resolve_embedding_model` 之后）追加。

先在文件顶部 `use` 区确认引入（若缺失则加）：

```rust
use crate::model_provider::ResolvedForProbe;
```

然后在 `impl ModelProviderStore` 内追加方法：

```rust
    /// 探测专用解析：不走 cache、不过滤启用状态、不回退。
    ///
    /// 按 model_id 直接从 DB 取该模型 + 其供应商（解密 api_key）。
    /// - 模型不存在 → Err
    /// - 模型/供应商被禁用 → 仍正常返回（探测的核心场景就是测这些）
    pub async fn resolve_for_probe(&self, model_id: &str) -> Result<ResolvedForProbe, AppError> {
        let mut conn = self.get_conn().await?;

        // 一条 JOIN 查询：取模型行 + 其供应商行（不论启用状态）
        #[derive(diesel::QueryableByName)]
        struct ProbeRow {
            #[diesel(sql_type = diesel::sql_types::Varchar)]
            model_id: String,
            #[diesel(sql_type = diesel::sql_types::Varchar)]
            model_name: String,
            #[diesel(sql_type = diesel::sql_types::Varchar)]
            model: String,
            #[diesel(sql_type = diesel::sql_types::Text)]
            tags: String,
            #[diesel(sql_type = diesel::sql_types::Varchar)]
            provider_name: String,
            #[diesel(sql_type = diesel::sql_types::Varchar)]
            vendor_name: String,
            #[diesel(sql_type = diesel::sql_types::Varchar)]
            base_url: String,
            #[diesel(sql_type = diesel::sql_types::Varchar)]
            protocol: String,
            #[diesel(sql_type = diesel::sql_types::Text)]
            encrypted_key: String,
        }

        let rows = diesel::sql_query(
            r#"
            SELECT m.id AS model_id, m.name AS model_name, m.model AS model, m.tags AS tags,
                   p.name AS provider_name, p.vendor_name AS vendor_name,
                   p.base_url AS base_url, p.protocol AS protocol, p.encrypted_key AS encrypted_key
            FROM llm_models m
            INNER JOIN llm_providers p ON p.id = m.provider_id
            WHERE m.id = $1
            LIMIT 1
            "#,
        )
        .bind::<diesel::sql_types::Text, _>(model_id)
        .get_results::<ProbeRow>(&mut conn)
        .await?;

        let row = rows.into_iter().next().ok_or_else(|| {
            AppError::BusinessError("模型不存在（可能已被删除）".into())
        })?;

        let api_key = if row.encrypted_key.is_empty() {
            String::new()
        } else {
            self.codec.decrypt(&row.encrypted_key).map_err(|e| {
                tracing::error!("[ModelProvider] 探测解析：模型 {} 的 API Key 解密失败: {}", row.model_id, e);
                AppError::BusinessError("API Key 解密失败，请检查服务端安全配置".into())
            })?
        };

        Ok(ResolvedForProbe {
            id: row.model_id,
            name: row.model_name,
            model: row.model,
            provider_name: row.provider_name,
            vendor_name: row.vendor_name,
            base_url: row.base_url,
            api_key,
            protocol: ProviderProtocol::parse(&row.protocol),
            tags: parse_tags(&row.tags),
        })
    }
```

确认 `cache.rs` 顶部已有 `use super::{... parse_tags ...}` 与 `use crate::model_provider::enums::{ProviderProtocol, Status}`（现有代码已含，无需改动）。若 `parse_tags` 未在 `use super::{}` 中，则补上（参考现有 `use super::{Cache, CachedModel, ModelProviderStore, ResolvedEmbeddingConfig, ResolvedLlmConfig, parse_tags};`）。

- [ ] **Step 2: 确认 mod.rs re-export 含 ResolvedForProbe**

确认 `src/model_provider/mod.rs` 含（Task 1 已加，此处核对）：

```rust
pub use probe::{ProbeKind, ProbeResult, ProbeStatus, ResolvedForProbe};
```

- [ ] **Step 3: 编译检查**

Run: `cargo check --bin cortex-agent 2>&1 | tail -10`
Expected: 通过。若有 "cannot find `parse_tags`" 之类，按提示补 use。

- [ ] **Step 4: Commit**

```bash
git add src/model_provider/store/cache.rs src/model_provider/mod.rs
git commit -m "$(cat <<'EOF'
feat(store): resolve_for_probe 探测专用解析(绕过缓存/回退)

按 model_id 一条 JOIN 查询取模型+供应商(不论启用状态),解密 api_key,
不过滤不回退。模型不存在返回 Err,被禁用仍返回——探测核心场景。

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: 后端编排 + DTO + GraphQL resolver

**Files:**
- Modify: `src/model_provider/dto.rs`
- Modify: `src/server/model_provider.rs`
- Modify: `src/server/graphql.rs`

**Interfaces:**
- Consumes: `ModelProviderStore::resolve_for_probe`、`probe::probe_one`、`probe::PROBE_TIMEOUT`、`ProbeResult`/`ProbeStatus`/`ProbeKind`
- Produces: `ProbeModelsInput { ids: Vec<String> }`、`pub async fn probe_models(state, req) -> Value`、GraphQL mutation `probeModels(input: JSON!)`

> DTO、编排、resolver 三者互相引用（resolver → probe_models → ProbeModelsInput），必须同一次编译通过、同一个 commit，无法拆分独立提交，故合并为一个任务。

- [ ] **Step 1: 新增 ProbeModelsInput DTO**

在 `src/model_provider/dto.rs`（文件末尾 `ModelOptionResponse` 之后）追加：

```rust
// ========== 模型探测 ==========

/// 批量探测请求（ids 为模型 id 列表）
#[derive(Debug, Deserialize)]
pub struct ProbeModelsInput {
    pub ids: Vec<String>,
}
```

- [ ] **Step 2: 实现 probe_models 编排**

在 `src/server/model_provider.rs` 顶部已有的 `use crate::model_provider::dto::{...}` 中加入 `ProbeModelsInput`：

```rust
use crate::model_provider::dto::{
    CreateModelRequest, CreateProviderRequest, ProbeModelsInput, ResetKeyRequest,
    UpdateModelRequest, UpdateProviderRequest,
};
```

在 `set_embedding_default` 函数之后追加编排（项目已依赖 futures，直接用全路径 `futures::future::join_all`）：

```rust
/// 批量探测模型存活（全并发，单模型 30s 超时）
pub async fn probe_models(state: &AppState, req: ProbeModelsInput) -> Value {
    let Some(store) = state.model_provider_store.as_ref() else {
        return db_unavailable();
    };
    if req.ids.is_empty() {
        return response::err(code::INVALID_PARAMS, "ids 不能为空");
    }

    // 全并发：每个 id 独立 resolve + probe + 超时
    let futs = req.ids.iter().map(|id| probe_one_id(store, id));
    let results = futures::future::join_all(futs).await;
    ok(json!({ "results": results }))
}

/// 单个 id 的探测（resolve 失败也产出 Fail 结果，不阻断整体）
async fn probe_one_id(
    store: &crate::model_provider::store::ModelProviderStore,
    id: &str,
) -> crate::model_provider::ProbeResult {
    match store.resolve_for_probe(id).await {
        Ok(resolved) => crate::model_provider::probe::probe_one(
            &resolved,
            crate::model_provider::probe::PROBE_TIMEOUT,
        )
        .await,
        Err(e) => crate::model_provider::ProbeResult {
            model_id: id.to_string(),
            model: String::new(),
            provider_name: String::new(),
            status: crate::model_provider::ProbeStatus::Fail,
            latency_ms: 0,
            probe_kind: crate::model_provider::ProbeKind::Chat,
            error: Some(e.to_string()),
            probed_at: chrono::Utc::now().to_rfc3339(),
        },
    }
}
```

- [ ] **Step 3: 在 graphql.rs MutationRoot 注册 probe_models**

在 `src/server/graphql.rs` 的 `impl MutationRoot`（模型供应商区段，`set_embedding_default_model` 之后、MCP 区段之前）插入：

```rust
    /// 批量探测模型存活（全并发，单模型 30s 超时）
    async fn probe_models(&self, ctx: &Context<'_>, input: Json) -> Json {
        let req: crate::model_provider::dto::ProbeModelsInput =
            match serde_json::from_value(input.0) {
                Ok(r) => r,
                Err(e) => return parse_err(e),
            };
        Json(super::model_provider::probe_models(state_of(ctx), req).await)
    }
```

- [ ] **Step 4: 编译 + 全量测试 + Commit**

Run: `cargo check --bin cortex-agent 2>&1 | tail -10` → Expected: 通过。
Run: `cargo test --lib 2>&1 | tail -15` → Expected: 全部 PASS（probe 模块测试不受影响）。

```bash
git add src/model_provider/dto.rs src/server/model_provider.rs src/server/graphql.rs
git commit -m "$(cat <<'EOF'
feat(probe): probe_models 编排 + ProbeModelsInput DTO + GraphQL probeModels

全并发(join_all)探测,单模型 30s 超时,resolve 失败产 Fail 不阻断整体。
DTO + 编排 + resolver 三者互相引用,同次编译通过同次提交。

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: 后端端到端手动验证

**Files:** 无（验证 Task 5/6/7 落地）

- [ ] **Step 1: 启动后端**

按 memory `local-run-setup` 启动（Windows + WSL Docker，需 `--bin cortex-agent`）。

Run（在项目根，按实际启动方式）: 启动 cortex-agent 进程，确认日志 `[ModelProvider] 初始化完成`。

- [ ] **Step 2: 用 curl/前端开发者工具验证 GraphQL 接口**

构造一个已存在模型的 id（从 `model_providers` query 拿一个），调用：

```bash
curl -s -X POST http://localhost:<port>/api/graphql \
  -H 'Content-Type: application/json' \
  -d '{"query":"mutation($input:JSON!){ probeModels(input:$input) }","variables":{"input":{"ids":["<真实模型id>"]}}}'
```

Expected: 返回 `{"data":{"probeModels":{"code":0,"message":"","data":{"results":[{"model_id":"...","status":"ok"|"fail",...}]}}}}`。
- 若该模型真实可用 → `"status":"ok"` + `latency_ms` 有值。
- 若 key/base_url 错 → `"status":"fail"` + `error` 含可操作文案。
- 故意传一个不存在的 id → `"status":"fail"` + `error` 含"模型不存在"。

- [ ] **Step 3: 验证禁用模型可探测**

在管理页禁用某模型，再用其 id 探测 → 应仍返回结果（不为空、不回退到默认模型）。

> 若验证失败，回到 Task 5/6 排查。验证通过后无需 commit（无代码变更）。

---

## Task 8: 前端 API 封装 + 选中/状态管理

**Files:**
- Modify: `frontend/src/api/index.js`
- Modify: `frontend/src/views/ModelProviderPage.vue`（仅 `<script setup>` 区的状态与函数，UI 在 Task 9）

**Interfaces:**
- Consumes: `gql()`
- Produces: `probeModels(ids)`、组件内 `selectedIds`(Set)、`probeStatusMap`(Map)、`probeResults`(ref)、`probeDrawerVisible`(ref)、`probeLoading`(ref)、`runProbe(ids)` 函数

- [ ] **Step 1: 新增 API 封装**

在 `frontend/src/api/index.js` 的「Model Provider」区段末尾（`setEmbeddingDefaultModel` 之后）追加：

```js
export const probeModels = (ids) =>
  gql(`mutation($input: JSON!) { probeModels(input: $input) }`, { input: { ids } })
```

- [ ] **Step 2: 在 ModelProviderPage.vue 引入 API + 新增状态**

修改 `frontend/src/views/ModelProviderPage.vue` 的 `<script setup>`。

在 import 块的 api 列表中加入 `probeModels`（原 import 自 `'../api'`）：

```js
import {
  fetchModelProviders,
  createModelProvider,
  updateModelProvider,
  deleteModelProvider,
  resetModelProviderKey,
  createModel,
  updateModel,
  deleteModel,
  setDefaultModel,
  setEmbeddingDefaultModel,
  probeModels,
} from '../api'
```

在 `const expandedKeys = ref([])` 之后新增探测相关状态：

```js
// ========== 模型探测 ==========
const selectedIds = ref(new Set())          // 跨供应商勾选的模型 id
const probeStatusMap = ref(new Map())       // model id -> {status:'probing'|'ok'|'fail', latency, error, kind}
const probeResults = ref([])                 // 最近一次探测结果数组（结果抽屉用）
const probeDrawerVisible = ref(false)
const probeLoading = ref(false)
```

- [ ] **Step 3: 实现选中与探测函数**

在 `<script setup>` 的 `reload` 函数之前新增：

```js
// ===== 选中管理 =====
function toggleSelect(id, checked) {
  const s = new Set(selectedIds.value)
  if (checked) s.add(id)
  else s.delete(id)
  selectedIds.value = s
}
function isSelected(id) {
  return selectedIds.value.has(id)
}
const selectedCount = computed(() => selectedIds.value.size)

// 刷新列表后与当前可见模型取交集，剔除已删除模型的残留选中
function pruneSelectedByVisible() {
  const visible = new Set()
  for (const p of providers.value) {
    for (const m of p.models || []) visible.add(m.id)
  }
  const s = new Set()
  for (const id of selectedIds.value) if (visible.has(id)) s.add(id)
  selectedIds.value = s
}

// ===== 探测 =====
async function runProbe(ids) {
  if (!ids || ids.length === 0) return
  probeLoading.value = true
  // 立即把待探测项置为 probing，UI 转圈
  const m = new Map(probeStatusMap.value)
  for (const id of ids) m.set(id, { status: 'probing' })
  probeStatusMap.value = m
  try {
    const { data, code, message } = await probeModels(ids)
    if (code === 0) {
      const results = (data && data.results) || []
      probeResults.value = results
      // 回填徽标状态
      const m2 = new Map(probeStatusMap.value)
      for (const r of results) {
        m2.set(r.model_id, {
          status: r.status, // 'ok' | 'fail'
          latency: r.latency_ms,
          error: r.error,
          kind: r.probe_kind,
        })
      }
      probeStatusMap.value = m2
      probeDrawerVisible.value = true
      const failCount = results.filter((r) => r.status === 'fail').length
      if (failCount === 0) ElMessage.success(`探测完成：${results.length} 个全部存活`)
      else ElMessage.warning(`探测完成：${failCount} 个失败，详见结果面板`)
    } else {
      ElMessage.error(message || '探测失败')
    }
  } catch (e) {
    ElMessage.error('请求失败: ' + (e.message || '网络错误'))
  } finally {
    probeLoading.value = false
  }
}

async function probeSelected() {
  await runProbe(Array.from(selectedIds.value))
}
function probeProviderAll(row) {
  const ids = (row.models || []).map((m) => m.id)
  if (ids.length === 0) return
  runProbe(ids)
}
async function probeOneModel(m) {
  await runProbe([m.id])
}
```

- [ ] **Step 4: 刷新后清理选中**

修改 `reload` 函数，在 `await loadProviders()` 之后调用 `pruneSelectedByVisible()`：

```js
async function reload() {
  await loadProviders()
  pruneSelectedByVisible()
  await appStore.loadModels()
}
```

- [ ] **Step 5: 前端构建检查 + Commit**

Run: `cd frontend && npm run build 2>&1 | tail -10`
Expected: 构建通过（无语法错误；UI 未接，仅逻辑层）。

```bash
git add frontend/src/api/index.js frontend/src/views/ModelProviderPage.vue
git commit -m "$(cat <<'EOF'
feat(frontend): 探测 API 封装 + 选中/状态管理逻辑

probeModels gql 封装;selectedIds(跨供应商 Set)、probeStatusMap(徽标)、
probeResults(抽屉)、runProbe(并发回填+消息提示)。UI 接线在下一步。

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 9: 前端 UI 集成（复选框 + 工具栏按钮 + 徽标 + 结果抽屉）

**Files:**
- Modify: `frontend/src/views/ModelProviderPage.vue`（`<template>` + `<style>`）

- [ ] **Step 1: 嵌套模型表加复选框列 + 探测徽标列**

在 `ModelProviderPage.vue` 的嵌套 `<el-table>`（`row.models` 那张，约第 57-122 行）中，在「默认」列之前插入复选框列，并在「状态」列后插入探测徽标列。

找到嵌套表的第一列：

```vue
              <el-table-column label="默认" width="100" align="center">
```

在其**之前**插入复选框列：

```vue
              <el-table-column label="" width="45" align="center">
                <template #default="{ row: m }">
                  <el-checkbox
                    :model-value="isSelected(m.id)"
                    @change="(v) => toggleSelect(m.id, v)"
                    @click.stop
                  />
                </template>
              </el-table-column>
```

找到嵌套表的「状态」列（`el-switch` 那列）之后、「更新时间」列之前，插入探测徽标列：

```vue
              <el-table-column label="探测" width="120" align="center">
                <template #default="{ row: m }">
                  <span v-if="!probeStatusMap.get(m.id)" class="cell-muted">—</span>
                  <span v-else-if="probeStatusMap.get(m.id).status === 'probing'" class="probe-probing">
                    <el-icon class="is-loading"><Loading /></el-icon> 探测中
                  </span>
                  <span v-else-if="probeStatusMap.get(m.id).status === 'ok'" class="probe-ok">
                    ✅ {{ probeStatusMap.get(m.id).latency }}ms
                  </span>
                  <span v-else class="probe-fail" :title="probeStatusMap.get(m.id).error">
                    ❌ 失败
                  </span>
                </template>
              </el-table-column>
```

在嵌套表操作列（`label="操作"`，width=160）的「编辑」按钮**之前**加单个探测按钮：

```vue
                    <div class="row-actions" @click.stop>
                      <el-button size="small" @click="probeOneModel(m)">探测</el-button>
                      <el-button size="small" @click="openModelDialog(row.id, m)">编辑</el-button>
                      ...（原有删除 popconfirm 不变）
```

并把操作列 width 从 160 调到 220 以容纳新按钮：

```vue
                <el-table-column label="操作" width="220" align="center" fixed="right">
```

- [ ] **Step 2: 工具栏加「探测选中」按钮 + 嵌套表头加「探测全部」**

在顶部工具栏右侧（`page-toolbar-right`，约第 21-28 行），「新建供应商」与「刷新」之间插入探测选中按钮：

```vue
      <div class="page-toolbar-right">
        <el-button type="primary" size="small" @click="openProviderDialog()">
          <el-icon><Plus /></el-icon> 新建供应商
        </el-button>
        <el-button
          size="small"
          type="success"
          plain
          :disabled="selectedCount === 0"
          :loading="probeLoading"
          @click="probeSelected"
        >
          <el-icon><Connection /></el-icon> 探测选中({{ selectedCount }})
        </el-button>
        <el-button size="small" @click="loadProviders" :loading="loading">
          <el-icon><Refresh /></el-icon> 刷新
        </el-button>
      </div>
```

在嵌套表头（`model-nested-head`，约第 51-56 行），「添加模型」按钮之前插入「探测本供应商全部」：

```vue
              <div class="model-nested-head">
                <span class="model-nested-title">模型列表（{{ row.models.length }}）</span>
                <div>
                  <el-button
                    size="small"
                    type="success"
                    plain
                    :disabled="(row.models || []).length === 0 || probeLoading"
                    @click="probeProviderAll(row)"
                  >
                    <el-icon><Connection /></el-icon> 探测本供应商全部
                  </el-button>
                  <el-button size="small" type="primary" plain @click="openModelDialog(row.id)">
                    <el-icon><Plus /></el-icon> 添加模型
                  </el-button>
                </div>
              </div>
```

- [ ] **Step 3: 新增图标 import**

在 `<script setup>` 的 icon import 行补充 `Loading`、`Connection`：

```js
import { Search, Refresh, Plus, InfoFilled, Loading, Connection } from '@element-plus/icons-vue'
```

- [ ] **Step 4: 添加结果抽屉面板**

在模板末尾（最后一个 `</el-dialog>` 之后、`</div>` page-root 之前）插入结果抽屉：

```vue
    <!-- 探测结果抽屉 -->
    <el-drawer
      v-model="probeDrawerVisible"
      title="探测结果"
      direction="rtl"
      size="560px"
    >
      <el-table :data="probeResults" border size="small">
        <el-table-column label="模型" min-width="160" show-overflow-tooltip>
          <template #default="{ row }">
            <div class="cell-title">{{ row.model || row.model_id }}</div>
            <div class="cell-muted" style="font-size:11px;">{{ row.provider_name }}</div>
          </template>
        </el-table-column>
        <el-table-column label="类型" width="90" align="center">
          <template #default="{ row }">
            <el-tag size="small" effect="plain">{{ row.probe_kind }}</el-tag>
          </template>
        </el-table-column>
        <el-table-column label="状态" width="90" align="center">
          <template #default="{ row }">
            <el-tag v-if="row.status === 'ok'" type="success" size="small" effect="dark">✅ 存活</el-tag>
            <el-tag v-else type="danger" size="small" effect="dark">❌ 失败</el-tag>
          </template>
        </el-table-column>
        <el-table-column label="耗时" width="80" align="center">
          <template #default="{ row }">
            <span class="cell-muted">{{ row.latency_ms }}ms</span>
          </template>
        </el-table-column>
        <el-table-column label="错误详情" min-width="200">
          <template #default="{ row }">
            <div v-if="row.error" class="probe-error-cell">
              <span class="probe-error-text">{{ row.error }}</span>
              <el-button size="small" text @click="copyError(row.error)">复制</el-button>
            </div>
            <span v-else class="cell-muted">—</span>
          </template>
        </el-table-column>
      </el-table>
    </el-drawer>
```

- [ ] **Step 5: 添加 copyError 函数 + 样式**

在 `<script setup>` 末尾（`formatTime` 附近）加复制函数：

```js
async function copyError(text) {
  try {
    await navigator.clipboard.writeText(text || '')
    ElMessage.success('错误信息已复制')
  } catch {
    ElMessage.error('复制失败，请手动选择文本复制')
  }
}
```

在 `<style scoped>` 末尾追加：

```css
.probe-probing { color: var(--accent); font-size: 12px; display: inline-flex; align-items: center; gap: 3px; }
.probe-ok { color: #67c23a; font-size: 12px; }
.probe-fail { color: #f56c6c; font-size: 12px; cursor: help; }
.probe-error-cell { display: flex; flex-direction: column; gap: 4px; }
.probe-error-text {
  font-size: 12px;
  color: #f56c6c;
  white-space: pre-wrap;
  word-break: break-all;
  max-height: 120px;
  overflow-y: auto;
}
```

- [ ] **Step 6: 前端构建 + 手动验证 + Commit**

Run: `cd frontend && npm run build 2>&1 | tail -10` → Expected: 构建通过。

手动验证（按 `local-run-setup` 跑前后端）：
- 进模型供应商页，展开某供应商，勾选若干模型（可跨供应商）→ 工具栏「探测选中(N)」数字正确。
- 点「探测选中」→ 各模型行「探测」列转圈，完成后变 ✅耗时 / ❌失败；右侧抽屉弹出结果列表。
- 点单模型「探测」按钮、点「探测本供应商全部」→ 行为正确。
- 失败项点「复制」→ 错误文本进剪贴板。
- 删除某已选中模型后刷新 → 工具栏选中数自动减少（交集剔除）。

```bash
git add frontend/src/views/ModelProviderPage.vue
git commit -m "$(cat <<'EOF'
feat(frontend): 探测 UI(复选框/工具栏按钮/徽标/结果抽屉)

嵌套模型表加复选框列(跨供应商勾选)+探测徽标列;工具栏探测选中(N)+
嵌套表头探测本供应商全部+行内单探测;右侧 el-drawer 结果面板(状态/
耗时/错误可复制)。刷新后选中与可见模型取交集剔除残留。

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## 完成标准

- [ ] `cargo test --lib` 全绿（probe 模块纯函数 + 执行器 wiremock 测试）。
- [ ] `cargo check --bin cortex-agent` 通过。
- [ ] `cd frontend && npm run build` 通过。
- [ ] 手动验证（Task 7 + Task 9 Step 6）：单模型/批量/全供应商探测、禁用模型可探测、协议冲突提示、超时提示、错误复制均正常。
