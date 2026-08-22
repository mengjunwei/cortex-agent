//! OpenAI 协议自动协商 — 优先 Responses API，回落 chat completions
//!
//! 用户配置供应商时只选 `openai_compat`，无需手动区分端点是否支持 OpenAI
//! Responses API（`/responses`）。本模块在首次真实调用前发一个最小探测请求：
//! 端点返回 2xx → 走 adk-rust 的 `OpenAIResponsesClient`（结构化 function
//! calling、原生 reasoning summary → `Part::Thinking`、不经过 ToolCallBuffer）；
//! 否则回落到本地 `OpenAICustomCompatible`（`/chat/completions`）。
//!
//! 探测结果按 `base_url|model|SHA-256(api_key)` 全局缓存（进程生命周期，key 含
//! api_key 哈希：同网关不同 key 的路由 ACL 可能不同；只存哈希不存明文，对齐
//! api_key 不落明文的基线）。结论分级：路由不存在（404/405/501）与无权访问
//! （401/403）是可缓存的否定；429/5xx/400 与网络类失败视为瞬时——本次回落
//! compat 并写 60s 短 TTL 负缓存（否则持续抖动的端点会在**每次**真实调用前
//! 白付一次最长 10s 的探测），TTL 过期后下次调用重探，网关恢复即可自愈。
//!
//! 误判可自愈：responses 路径运行时若收到 401/403/404/405/501，或响应体解析
//! 失败（parse error——非 JSON 错误体经 async-openai 转为 JSONDeserialize 后
//! **不带 upstream status**，只有错误码可判），写否定缓存并立即用 compat 路径
//! 重试本次请求。翻转带冷却期而非永久（单次瞬时 404 不得永久钉死多后端负载
//! 均衡端点）：带 status 的错误用 60s 短冷却；parse 类格式漂移用 1h 长冷却，
//! 防止「探测过、真实调用必失败」的端点 60s 周期振荡双请求双计费。
//!
//! 子模块：
//! - [`probe`]：端点探测、三态结论缓存、自愈降级判定；
//! - [`usage`]：/responses usage 口径归一（净输入动态判定 + gross 折算）。
//!
//! 已知取舍（审查记录，暂不处理）：
//! - 上游 responses 客户端流式路径无内部重试（仅外层 generate_with_retry 的
//!   3 次短退避兜底；compat 路径内部有 5 次长退避重试）；
//! - 运行时 client 无法注入 no-proxy 配置（上游无注入口），loopback 探测直连但
//!   运行时仍可能走系统代理；
//! - 探测 POST 在 OpenAI 官方端点会创建一个服务端存储的 Response 对象
//!   （store 默认 true；不加 "store": false 以兼容拒绝未知字段的严格网关）；
//! - Responses API 无 frequency_penalty / presence_penalty 参数，上游也不读
//!   （cortex 默认注入 0.4/0.3 的防复读惩罚在 responses 路径静默失效——推理
//!   模型本就常忽略该参数，弱模型请配在无 /responses 的网关上或关闭协商）；
//! - 上游不读 `config.response_schema`（Responses API 用 text.format 表达），
//!   带 schema 的请求在协商层直走 compat 路径，保证结构化输出不失效。
//!
//! 一键关闭：环境变量 `CORTEX_DISABLE_OPENAI_RESPONSES=1`（构造期读取，
//! 关闭后行为与改动前完全一致）。

mod probe;
mod usage;

pub use probe::disabled_by_env;

use std::time::{Duration, Instant};

use adk_rust::model::openai::OpenAIResponsesClient;
use adk_rust::{AdkError, Llm, LlmRequest, LlmResponseStream};
use async_trait::async_trait;
use futures::StreamExt;
use sha2::{Digest, Sha256};

use crate::llm::openai::compat::OpenAICustomCompatible;
use probe::{
    detect_responses_support, downgrade_cooldown, error_brief, is_unsupported_responses_error,
    ProbeCacheEntry, ProbeVerdict, RESPONSES_SUPPORT, RETRY_PROBE_AFTER,
};
use usage::{conv_fingerprint, normalize_responses_usage};

/// 协商后的 OpenAI 客户端：首次调用 lazily 决定走 Responses 还是 chat completions。
pub struct OpenAiAutoLlm {
    compat: OpenAICustomCompatible,
    responses: OpenAIResponsesClient,
    cache_key: String,
    base_url: String,
    api_key: String,
    model: String,
}

impl OpenAiAutoLlm {
    /// 组装协商客户端。两个底层客户端由调用方构造（复用统一的 retry 配置）。
    pub fn new(
        compat: OpenAICustomCompatible,
        responses: OpenAIResponsesClient,
        base_url: &str,
        api_key: &str,
        model: &str,
    ) -> Self {
        // key 只存 api_key 的 SHA-256（hex，与 domain/auth/api_token.rs 的
        // sha256_hex 同法）：同 key 必同哈希（隔离语义不变），但明文不进
        // 全局 static HashMap（见 RESPONSES_SUPPORT 文档）。
        let api_key_hash = Sha256::digest(api_key.as_bytes());
        let api_key_hex: String = api_key_hash.iter().map(|b| format!("{b:02x}")).collect();
        Self {
            compat,
            responses,
            cache_key: format!(
                "{}|{}|{}",
                base_url.trim_end_matches('/'),
                model,
                api_key_hex
            ),
            base_url: base_url.to_string(),
            api_key: api_key.to_string(),
            model: model.to_string(),
        }
    }

    /// 查询（必要时探测）该端点是否优先走 /responses。
    ///
    /// 探测在锁外进行：全局锁若跨探测 await（最长 10s 超时），一个慢端点会阻塞
    /// 所有无关供应商的首次调用。锁外探测的竞态至多造成同一端点被并发探测几次，
    /// 幂等无害。写回时已有的高置信结论（Supported/Unsupported）不被并发探测
    /// 覆盖——只有仍处于否定窗口的条目可被更新（见下方守卫）。
    async fn prefer_responses(&self) -> bool {
        {
            let cache = RESPONSES_SUPPORT.lock().await;
            if let Some(entry) = cache.get(&self.cache_key) {
                if let Some(prefer) = entry.resolve(Instant::now()) {
                    return prefer;
                }
                // 否定窗口过期：不在此删（写回段守卫兜底），锁外重探
            }
        }
        let verdict = detect_responses_support(&self.base_url, &self.api_key, &self.model).await;
        // 确定性结论(Supported/Unsupported 长期缓存,进程内首见)记 info;
        // Transient 只有 60s 负缓存——持续 429/5xx 的端点每分钟重探一次,
        // 重复记 info 会刷屏,降 debug。
        match verdict {
            ProbeVerdict::Transient => tracing::debug!(
                "[openai-auto] /responses 探测(瞬时,60s 后重试): {} model={}",
                self.name(),
                self.model
            ),
            other => tracing::info!(
                "[openai-auto] /responses 探测完成: {} model={} => {:?}",
                self.name(),
                self.model,
                other
            ),
        }
        let entry = match verdict {
            ProbeVerdict::Supported => ProbeCacheEntry::Supported,
            // 确定性否定（404/401 等稳定状态）：长期缓存
            ProbeVerdict::Unsupported => ProbeCacheEntry::Unsupported,
            // 瞬时/不确定：写短 TTL 负缓存——不写的话持续 429/5xx/黑洞路由
            // 的端点会在每次真实调用前都白付一次探测往返
            ProbeVerdict::Transient => {
                ProbeCacheEntry::NegativeUntil(Instant::now() + RETRY_PROBE_AFTER)
            }
        };
        let now = Instant::now();
        let mut cache = RESPONSES_SUPPORT.lock().await;
        match cache.get(&self.cache_key) {
            // 高置信结论已落缓存（探测期间并发写入）：慢探测不得覆盖——否则
            // 一个抖动的 Transient 会把 Supported=true 打成 60s compat。且本次
            // 路由须跟缓存一致（自判可能过时），返回缓存结论而非本地 verdict。
            Some(existing @ (ProbeCacheEntry::Supported | ProbeCacheEntry::Unsupported)) => {
                return existing.resolve(now).unwrap_or(false);
            }
            // 否定窗口内的条目：运行时自愈写入的失败实证优先于探测推断——
            // 同为否定时保留更晚到期的冷却，防止慢探测的 Transient（60s）把
            // 并发自愈刚写入的长冷却（1h）缩短、复活振荡；探测的 Supported
            // 同样不得覆盖**未过期**的自愈否定（真实调用失败比探测推断强），
            // 只有过期否定（本就该重探）才允许翻转为肯定
            Some(ProbeCacheEntry::NegativeUntil(existing_t)) => {
                let merged = match entry {
                    ProbeCacheEntry::NegativeUntil(t) => {
                        ProbeCacheEntry::NegativeUntil(t.max(*existing_t))
                    }
                    ProbeCacheEntry::Supported if now < *existing_t => {
                        ProbeCacheEntry::NegativeUntil(*existing_t)
                    }
                    other => other,
                };
                cache.insert(self.cache_key.clone(), merged);
            }
            // 无条目：写入本次结论
            None => {
                cache.insert(self.cache_key.clone(), entry);
            }
        }
        entry.resolve(Instant::now()).unwrap_or(false)
    }

    /// 运行时自愈：responses 路径判明端点不支持后，写否定缓存令后续调用直走
    /// compat。带冷却期（而非永久）：多后端负载均衡网关单次瞬时 404 不得把
    /// 整个端点钉死在 compat 直到进程重启；parse 类格式漂移用更长冷却，防止
    /// 「探测过、真实调用失败」的端点 60s 周期性振荡双计费。
    async fn mark_unsupported(&self, cooldown: Duration) {
        let new_until = Instant::now() + cooldown;
        let mut cache = RESPONSES_SUPPORT.lock().await;
        // 同为否定时保留更晚到期（防止并发同类降级把长冷却缩短复活振荡）；
        // 探测的确定性否定 Unsupported（长期）不降级为带时限否定——自愈翻转
        // 只把低置信条目拉长，不缩短高置信否定。覆盖 Supported 方向正常
        // （真实调用失败是更新鲜的证据）。
        let merged = match cache.get(&self.cache_key) {
            Some(existing @ ProbeCacheEntry::Unsupported) => *existing,
            Some(ProbeCacheEntry::NegativeUntil(t)) if *t > new_until => {
                ProbeCacheEntry::NegativeUntil(*t)
            }
            _ => ProbeCacheEntry::NegativeUntil(new_until),
        };
        cache.insert(self.cache_key.clone(), merged);
    }
}

/// 把 chat 路径的 thinking 级别键转换为 Responses 路径的嵌套键。
///
/// - chat completions（openai_custom）：`extensions["openai"]["reasoning_effort"]`
///   （顶层字段合并进 body）
/// - Responses API（OpenAIResponsesClient）：`extensions["openai"]["reasoning"]["effort"]`
///
/// 转换放在协商层而非 make_gen_config_from：chat 路径会把整个
/// `extensions["openai"]` 对象合并进请求 body，config 里预塞嵌套 `reasoning`
/// 键会污染 `/chat/completions` 请求（GLM 等严格 API 可能 400）。
/// 仅认 low/medium/high（Responses 侧 xhigh 不受支持，跳过走模型默认）。
fn adapt_request_for_responses(mut request: LlmRequest) -> LlmRequest {
    let Some(config) = request.config.as_mut() else {
        return request;
    };
    // 先拷出 level（String），结束不可变借用，才能对 extensions 做 entry 可变借用
    let Some(level) = config
        .extensions
        .get("openai")
        .and_then(|o| o.get("reasoning_effort"))
        .and_then(|v| v.as_str())
        .filter(|l| matches!(*l, "low" | "medium" | "high"))
        .map(String::from)
    else {
        return request;
    };
    let entry = config
        .extensions
        .entry("openai".to_string())
        .or_insert_with(|| serde_json::json!({}));
    if let Some(obj) = entry.as_object_mut() {
        obj.insert(
            "reasoning".to_string(),
            serde_json::json!({ "effort": level }),
        );
    }
    request
}

/// 路由观测 info 去重：`(协议, name, model)` 维度首见返回 true。
///
/// 去重集合必须进程级共享——客户端实例随每次请求重建，放实例字段记不住；
/// resp/chat 各占一个 key 前缀，协议翻转（否定过期重探/运行时降级）时
/// 另一方向仍能首见记一次，日志可完整呈现路由变化。
fn route_info_first_seen(key: &str) -> bool {
    use std::collections::HashSet;
    use std::sync::{LazyLock, Mutex};
    static LOGGED: LazyLock<Mutex<HashSet<String>>> =
        LazyLock::new(|| Mutex::new(HashSet::new()));
    LOGGED
        .lock()
        .map(|mut set| set.insert(key.to_string()))
        .unwrap_or(false)
}

#[async_trait]
impl Llm for OpenAiAutoLlm {
    fn name(&self) -> &str {
        // 两个底层客户端的 name() 都返回模型名，取 compat 的即可
        self.compat.name()
    }

    async fn generate_content(
        &self,
        request: LlmRequest,
        stream: bool,
    ) -> Result<LlmResponseStream, AdkError> {
        // 带 response_schema 的请求直走 compat：上游 OpenAIResponsesClient 不读
        // `config.response_schema`（Responses API 用 text.format 表达，上游未实现），
        // 走 responses 路径会让结构化输出（assistant 生成 / FAQ 生成等）静默失效。
        if request
            .config
            .as_ref()
            .is_some_and(|c| c.response_schema.is_some())
        {
            // 与 /responses 端点并存的第三种路由：端点支持 responses 时同模型会
            // 同时出现两条 info，此条注明触发原因避免误读为端点不支持
            tracing::debug!(
                "[openai-auto] {} 结构化输出请求走 /chat/completions 协议(model={})",
                self.name(),
                self.model
            );
            if route_info_first_seen(&format!("schema-chat|{}|{}", self.name(), self.model)) {
                tracing::info!(
                    "[openai-auto] {} 结构化输出(response_schema)请求固定走 /chat/completions 协议(model={})",
                    self.name(),
                    self.model
                );
            }
            return self.compat.generate_content(request, stream).await;
        }
        if !self.prefer_responses().await {
            // 与 /responses 方向对称的 info 路由观测(首见记一次)
            tracing::debug!(
                "[openai-auto] {} 走 /chat/completions 协议(model={})",
                self.name(),
                self.model
            );
            if route_info_first_seen(&format!("chat|{}|{}", self.name(), self.model)) {
                // 不写「端点无 /responses」：Transient 负缓存（60s 重试窗口）也走
                // 此分支，措辞只陈述实际路由，不陈述原因
                tracing::info!(
                    "[openai-auto] {} 走 /chat/completions 协议(model={})",
                    self.name(),
                    self.model
                );
            }
            return self.compat.generate_content(request, stream).await;
        }
        // 路由观测:运维无法从外部判断供应商实际在用哪个协议,resp/chat 两方向
        // 都必须有 info。debug 每次记(定位单次调用),info 仅模型名维度首见记
        // 一次(避免刷屏),见 route_info_first_seen。
        tracing::debug!(
            "[openai-auto] {} 走 /responses 协议(model={})",
            self.name(),
            self.model
        );
        if route_info_first_seen(&format!("resp|{}|{}", self.name(), self.model)) {
            tracing::info!(
                "[openai-auto] {} 检测到 /responses 端点,后续调用走 Responses 协议(model={})",
                self.name(),
                self.model
            );
        }

        // 降级用副本必须在 adapt 之前留存：adapt 会注入嵌套 reasoning 键，
        // 若降级时把 adapted 请求发给 compat，该键会被整对象合并进
        // /chat/completions body（严格 API 400），自愈路径反而失败。
        // （上游 generate_content 按值消费请求，降级时拿不回 adapted 请求做
        // remove 还原——全量克隆是必要的，代价被降级低概率摊薄。）
        let compat_request = request.clone();
        // thinking 级别键转换（reasoning_effort → reasoning.effort），仅影响本路径
        let request = adapt_request_for_responses(request);
        // usage 口径归一的请求侧输入：contents 即实际发送的对话全量（adapt 只动
        // config extensions），指纹在请求消费前计算、随流闭包存活
        let usage_fp = conv_fingerprint(&request.contents);
        let usage_log_tag = format!("{} model={}", self.base_url, self.model);

        let inner = match self.responses.generate_content(request, stream).await {
            Ok(s) => s,
            // stream=true 时上游 create_stream 在 generate_content 内**急切** await，
            // 初始 HTTP 错误（含 404 端点不存在）直接以 Err 返回——由此分支降级；
            // 仅非流式路径的 HTTP 请求在 try_stream! 内惰性执行，初始错误延迟到
            // 流的首个 Err item，由下方流首拦截处理。两个分支互补，缺一不可。
            Err(e) if is_unsupported_responses_error(&e) => {
                tracing::warn!(
                    "[openai-auto] /responses 初始调用失败（{}），降级为 /chat/completions",
                    error_brief(&e)
                );
                self.mark_unsupported(downgrade_cooldown(&e)).await;
                return self.compat.generate_content(compat_request, stream).await;
            }
            Err(e) => return Err(e),
        };

        // 流首拦截 + 自愈降级：compat 客户端需 Clone 进 'static 流闭包
        let compat = self.compat.clone();
        let cache_key = self.cache_key.clone();
        let response_stream = async_stream::try_stream! {
            let mut source = inner;
            // usage 归一仅在 responses 源上生效：自愈换 compat 后 usage 来自
            // openai_custom（chat completions 的 prompt_tokens 本就 gross），不得折算
            let mut usage_fold_active = true;
            // 上游把（非流式路径的）初始 HTTP 错误延迟到流的首个 item：
            // 在此拦截，判明端点不支持后翻转缓存、整流切换到 compat 重发本次请求。
            if let Some(mut item) = source.next().await {
                if let Err(e) = &item
                    && is_unsupported_responses_error(e)
                {
                    tracing::warn!(
                        "[openai-auto] /responses 探测误判（{}），已降级为 /chat/completions 并更新缓存",
                        error_brief(e)
                    );
                    let mut cache = RESPONSES_SUPPORT.lock().await;
                    // 同为否定时保留更晚到期的冷却：并发同类降级（一个 parse→1h、
                    // 一个 404→60s）先后的覆盖不应把长冷却缩短复活振荡；探测的
                    // 确定性否定 Unsupported（长期）不降级为带时限否定。覆盖
                    // Supported/过期条目方向正常（真实调用失败是更新鲜的证据）
                    let new_until = Instant::now() + downgrade_cooldown(e);
                    let merged = match cache.get(&cache_key) {
                        Some(existing @ ProbeCacheEntry::Unsupported) => *existing,
                        Some(ProbeCacheEntry::NegativeUntil(t)) if *t > new_until => {
                            ProbeCacheEntry::NegativeUntil(*t)
                        }
                        _ => ProbeCacheEntry::NegativeUntil(new_until),
                    };
                    cache.insert(cache_key.clone(), merged);
                    drop(cache);
                    let compat_stream = compat.generate_content(compat_request, stream).await?;
                    source = compat_stream;
                    usage_fold_active = false;
                } else {
                    if usage_fold_active
                        && let Ok(resp) = item.as_mut()
                    {
                        normalize_responses_usage(resp, &usage_fp, &cache_key, &usage_log_tag);
                    }
                    yield item?;
                }
            }
            while let Some(mut item) = source.next().await {
                if usage_fold_active
                    && let Ok(resp) = item.as_mut()
                {
                    normalize_responses_usage(resp, &usage_fp, &cache_key, &usage_log_tag);
                }
                yield item?;
            }
        };
        Ok(Box::pin(response_stream))
    }
}

#[cfg(test)]
mod tests;
