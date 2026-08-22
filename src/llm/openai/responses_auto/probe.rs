//! /responses 端点探测、结论缓存与运行时自愈判定。
//!
//! 三态结论（[`ProbeVerdict`] → [`ProbeCacheEntry`]）与冷却策略的完整语义见
//! 各条目文档；探测在锁外进行（见 `OpenAiAutoLlm::prefer_responses`）。

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use adk_rust::AdkError;
use tokio::sync::Mutex;

/// 探测结论（含是否可缓存）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProbeVerdict {
    /// 2xx 且响应体为合法 Responses 对象 → 优先 /responses（可缓存）
    Supported,
    /// 端点明确没有 /responses 路由，或该 key 无权访问（401/403）→ 走 compat（可缓存；
    /// 401/403 是稳定状态：key 的路由 ACL 不随调用变化）
    Unsupported,
    /// 429/5xx/400/网络错误等瞬时或不确定状态 → 本次走 compat，写 60s 短 TTL
    /// 负缓存（RETRY_PROBE_AFTER），窗口内不重探，过期后下次调用重探
    Transient,
}

/// 瞬时否定的重探窗口：探测结论为 Transient 后 60s 内直走 compat 不重探，
/// 避免持续 429/5xx/黑洞路由的端点在每次真实调用前都白付一次最长 10s 的
/// 探测往返；窗口过期后重探，网关恢复即自愈。
pub(super) const RETRY_PROBE_AFTER: Duration = Duration::from_secs(60);

/// 运行时自愈翻转的冷却期：探测判「支持」但真实调用 parse 失败（格式漂移）
/// 时，短 TTL 会让端点陷入 60s 周期振荡——每窗口边界重探（最小探测请求
/// 恰好能过）又翻回 true、真实调用再失败、再翻 false，每次都双请求双计费。
/// parse 翻转用更长冷却，把振荡成本从 60s 一次摊薄到 1h 一次。
const HEAL_COOLDOWN: Duration = Duration::from_secs(3600);

/// 探测/协商结果缓存：key = `base_url|model|SHA-256(api_key)`，见
/// [`ProbeCacheEntry`] 的三态语义。
///
/// key 必须区分 api_key：多用户场景下同一网关不同 key 的路由 ACL 可能不同
/// （LiteLLM/one-api 常见），按端点共享结论会让无 /responses 权限的用户被
/// 别人的探测结果打挂，且 401/403 不在自愈降级集合里（无法恢复）。
/// 只存 SHA-256 哈希不存明文：api_key 静态存储全程 AES 加密（model_provider/
/// crypto.rs），缓存 key 若拼明文会让所有供应商 key 在进程内存驻留整个生命周期。
///
/// tokio Mutex 只保护 HashMap 的瞬时读写，探测请求本身在锁外发出（见
/// `OpenAiAutoLlm::prefer_responses`），避免慢端点探测串行化全局首次调用。
pub(super) static RESPONSES_SUPPORT: LazyLock<Mutex<HashMap<String, ProbeCacheEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 缓存条目（三态，含有效期）。
///
/// - `Supported`：探测 2xx 且合法 Responses 对象——高置信肯定，**长期有效**
///   （进程生命周期；误判由运行时自愈翻转兜底）；
/// - `Unsupported`：探测收到 404/405/501/401/403 等**确定性**否定——路由
///   不存在与 key 无权是稳定状态，同样**长期有效**（对齐 architecture.md
///   「肯定/明确否定结论长期有效」的承诺）；
/// - `NegativeUntil`：低置信否定（探测 Transient：429/5xx/网络错；或运行时
///   自愈翻转）——短/长 TTL 过期后重探。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum ProbeCacheEntry {
    Supported,
    Unsupported,
    NegativeUntil(Instant),
}

impl ProbeCacheEntry {
    /// 当前是否应走 /responses；None 表示否定已过期、应重探
    pub(super) fn resolve(self, now: Instant) -> Option<bool> {
        match self {
            ProbeCacheEntry::Supported => Some(true),
            ProbeCacheEntry::Unsupported => Some(false),
            ProbeCacheEntry::NegativeUntil(t) if now < t => Some(false),
            ProbeCacheEntry::NegativeUntil(_) => None,
        }
    }
}

/// 探测专用 HTTP 客户端（短超时，不重试——探测只应占首次调用前的几秒）
static DETECT_HTTP: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
});

/// loopback 专用探测客户端：完全禁用代理。
///
/// reqwest 默认吃 `HTTP(S)_PROXY` 环境变量——带代理环境下探测 Ollama 等本地
/// 端点会被代理拦截（502/504），误判为不支持。外部端点仍走 DETECT_HTTP（保留
/// 代理支持，公司网络可达性优先），仅 loopback 直连。
static DETECT_HTTP_DIRECT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(10))
        .no_proxy()
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
});

/// 目标是否为 loopback 端点（决定探测是否绕过系统代理）。
///
/// 用 `Url::host()` 匹配 `is_loopback()`：覆盖 127.0.0.0/8 整段（不只
/// 127.0.0.1——`http://127.0.0.2:11434` 的 Ollama 会被误判为外部端点走代理）
/// 与 IPv6 loopback 段。注意 `host_str()` 对 IPv6 恒带方括号序列化，不适合
/// 字符串匹配。
pub(super) fn is_loopback_url(url: &str) -> bool {
    match reqwest::Url::parse(url)
        .ok()
        .as_ref()
        .and_then(|u| u.host())
    {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        // 非 IP host（"localhost" 等 DNS 名）：只有 localhost 可确定指向本机
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        _ => false,
    }
}

/// 是否通过环境变量关闭自动协商（构造期读取一次）
pub fn disabled_by_env() -> bool {
    static DISABLED: LazyLock<bool> =
        LazyLock::new(|| match std::env::var("CORTEX_DISABLE_OPENAI_RESPONSES") {
            Ok(v) => matches!(v.trim(), "1" | "true" | "yes" | "on"),
            Err(_) => false,
        });
    *DISABLED
}

/// 发送最小 /responses 探测请求。
///
/// 返回 [`ProbeVerdict`]：`Supported`（2xx 且响应体是合法 Responses 对象）、
/// `Unsupported`（收到 HTTP 响应但不支持——非 2xx，或 2xx 但响应体不是
/// Responses 格式，后者见于 catch-all 路由的网关对任意 POST 都 200）、
/// `Transient`（网络层失败 / 429 / 5xx / 400 等瞬时状态，写短 TTL 负缓存，
/// 过期重探）。
pub(super) async fn detect_responses_support(
    base_url: &str,
    api_key: &str,
    model: &str,
) -> ProbeVerdict {
    let url = format!("{}/responses", base_url.trim_end_matches('/'));
    let client = if is_loopback_url(&url) {
        &*DETECT_HTTP_DIRECT
    } else {
        &*DETECT_HTTP
    };
    let body = serde_json::json!({
        "model": model,
        "input": "ping",
        "max_output_tokens": 16,
    });
    let mut req = client.post(&url).json(&body);
    // api_key 为空不发鉴权头（兼容 Ollama 等本地端点，对齐 probe 的做法）
    if !api_key.is_empty() {
        req = req.bearer_auth(api_key);
    }
    let resp = match req.send().await {
        Ok(r) => r,
        Err(_) => return ProbeVerdict::Transient, // 网络层失败：结论不可靠
    };
    // 结论分级：只有「路由不存在」（404/405/501）和「该 key 无权」（401/403）
    // 是可缓存的否定；429/5xx/400 是瞬时或不确定状态，缓存了会把可用端点
    // 永久打成 compat（违背本模块的瞬时故障不缓存策略）。
    match resp.status().as_u16() {
        404 | 405 | 501 | 401 | 403 => ProbeVerdict::Unsupported,
        400 | 408 | 429 => ProbeVerdict::Transient,
        s if (500..600).contains(&s) => ProbeVerdict::Transient,
        s if (200..300).contains(&s) => {
            // 响应体校验：合法 Responses 对象必含 "object":"response"（OpenAI /
            // vLLM 等实现一致）。防 catch-all 网关 200 返回 HTML/任意 JSON 造成
            // 误判——那会导致运行时 parse error（无 upstream status，自愈无法触发）。
            // body 读取失败（如超时打断传输）按 Transient 处理，不得缓存——
            // 否则满负载网关的慢响应会把可用的 /responses 永久打成 compat。
            let body = match resp.text().await {
                Ok(b) => b,
                Err(_) => return ProbeVerdict::Transient,
            };
            let is_responses_json = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| {
                    v.get("object")
                        .and_then(|o| o.as_str())
                        .map(|s| s == "response")
                })
                .unwrap_or(false);
            if is_responses_json {
                ProbeVerdict::Supported
            } else {
                ProbeVerdict::Unsupported
            }
        }
        // 其他非 2xx（3xx 重定向、代理鉴权跳转、维护页等）：状态语义不确定，
        // 不应永久钉死端点——按瞬时处理（短 TTL 负缓存，过期重探）
        _ => ProbeVerdict::Transient,
    }
}

/// 判断 responses 路径的错误是否应触发降级（compat 路径重发本次请求）。
///
/// - 404/405/501：路径不存在——探测误判，须降级并翻转缓存；
/// - 401/403：鉴权类失败也降级——空 key 端点探测时不发鉴权头、运行时上游却发
///   占位 bearer，可选鉴权网关会每次 401。注意降级后 compat 对空 key 仍发空
///   `Authorization: Bearer ` 头（send_request 无条件 bearer_auth），个别网关
///   会拒绝空 token——此时错误照常浮出，两路径都不可用本就不是协商层能解决的；
/// - parse 错误（`model.openai_responses.parse`）：端点返回的 body 不是合法
///   Responses 对象。非 JSON 错误体（nginx/one-api 纯文本 404 页）经
///   async-openai 转为 JSONDeserialize，**不带 upstream status**，404/401
///   分支对它们失明——parse 错误码是唯一可判的信号，必须触发降级，否则误判
///   端点每次调用都报 parse error 且永不恢复；
/// - 400/422（参数校验）、429、5xx 等可能是偶发问题，透传给上层报错更诚实。
pub(super) fn is_unsupported_responses_error(err: &AdkError) -> bool {
    if err.code == "model.openai_responses.parse" {
        return true;
    }
    matches!(
        err.details.upstream_status_code,
        Some(401 | 403 | 404 | 405 | 501)
    )
}

/// 降级翻转的冷却时长：parse 类（响应体格式漂移——探测的最小请求恰好能过、
/// 真实调用必失败）用长冷却，防止短 TTL 造成 60s 周期振荡（每窗口边界重探
/// 翻回 true、真实调用再失败、再翻 false，双请求双计费）；带 status 的
/// 404/401 等（瞬时路由漂移 / 鉴权不对称）用短冷却快速自愈。
pub(super) fn downgrade_cooldown(e: &AdkError) -> Duration {
    if e.details.upstream_status_code.is_some() {
        RETRY_PROBE_AFTER
    } else {
        HEAL_COOLDOWN
    }
}

/// 错误摘要（供降级日志）：status 码优先，parse 类无 status 的给错误码
pub(super) fn error_brief(e: &AdkError) -> String {
    match e.details.upstream_status_code {
        Some(s) => format!("HTTP {s}"),
        None => e.code.to_string(),
    }
}
