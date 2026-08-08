//! LLM 查询理解服务 — 从自然语言查询中提取结构化检索信息
//!
//! ## 工作流程
//!
//! ```text
//! 用户查询 → 查 LRU 缓存 → 命中则直接返回
//!                ↓ 未命中
//!           调用 LLM 提取 → StructuredQuery → 写入缓存 → 返回
//! ```
//!
//! 带有 LRU 缓存（默认 500 条）避免相同查询重复调用 LLM，降低延迟和成本。

use std::num::NonZeroUsize;
use std::sync::{Arc, RwLock};

use adk_rust::{Content, GenerateContentConfig, Llm, LlmRequest};
use futures::StreamExt;
use lru::LruCache;
use serde::Deserialize;

/// 结构化查询 — LLM 提取的检索意图
///
/// 从用户自然语言中提取的三元组，用于精确过滤知识库检索结果。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct StructuredQuery {
    /// 标准厂商名（如 Huawei、H3C、Cisco），无法确定时为 None
    pub brand: Option<String>,
    /// 标准设备类型（如 router、switch、firewall），无法确定时为 None
    pub dev_type: Option<String>,
    /// 设备型号（如 S5300、AR2220），无法确定时为 None
    #[serde(default)]
    pub model: Option<String>,
    /// 核心检索关键词（去除厂商/类型词后的纯净关键词）
    #[serde(default)]
    pub keywords: Vec<String>,
}

const UNDERSTAND_PROMPT: &str = r#"从用户查询中提取结构化检索信息，输出JSON。

规则：
- brand：标准英文厂商名（Huawei/H3C/Cisco/Ruijie/Juniper/Maipu/Fiberhome/ZTE），无法确定时为null
- dev_type：标准设备类型（router/switch/firewall/lb/ap/ac），无法确定时为null
- model：设备型号（如 S5300、AR2220、S5700、Cisco2960），从用户输入中提取，无法确定时为null
- keywords：核心检索词（去除厂商、设备类型和型号后的关键词，如"静态路由配置"）

示例：
"华为静态路由配置命令" → {"brand":"Huawei","dev_type":"router","model":null,"keywords":["静态路由","配置"]}
"h3c S5300交换机VLAN配置" → {"brand":"H3C","dev_type":"switch","model":"S5300","keywords":["VLAN","配置"]}
"防火墙安全策略" → {"brand":null,"dev_type":"firewall","model":null,"keywords":["安全策略"]}

只输出JSON，不要其他内容。"#;

/// LLM 查询理解服务（带 LRU 缓存）
///
/// 将自然语言查询转换为结构化的 `StructuredQuery`，用于精确过滤知识库检索结果。
/// 内部维护一个 LRU 缓存，避免对相同查询重复调用 LLM。
pub struct QueryUnderstandingService {
    model: Arc<dyn Llm>,
    cache: RwLock<LruCache<String, StructuredQuery>>,
}

impl QueryUnderstandingService {
    /// 创建查询理解服务
    ///
    /// # 参数
    /// - `model`：LLM 模型实例
    /// - `cache_size`：LRU 缓存容量（默认 500）
    pub fn new(model: Arc<dyn Llm>, cache_size: usize) -> Self {
        let cap = NonZeroUsize::new(cache_size).unwrap_or(NonZeroUsize::new(500).unwrap());
        Self {
            model,
            cache: RwLock::new(LruCache::new(cap)),
        }
    }

    /// 从自然语言查询中提取结构化信息
    ///
    /// 优先查 LRU 缓存（peek 不修改 LRU 顺序），未命中则调用 LLM 提取并写入缓存。
    /// 空查询直接返回默认值（所有字段为 None/空）。
    pub async fn understand(&self, query: &str) -> StructuredQuery {
        let query_trimmed = query.trim();
        if query_trimmed.is_empty() {
            return StructuredQuery::default();
        }

        // 1. 查缓存（peek 不修改 LRU 顺序）
        {
            let cache = self.cache.read().unwrap();
            if let Some(sq) = cache.peek(query_trimmed) {
                return sq.clone();
            }
        }

        // 2. 调 LLM 提取
        let sq = self.call_llm(query_trimmed).await;

        // 3. 写缓存
        {
            let mut cache = self.cache.write().unwrap();
            cache.put(query_trimmed.to_string(), sq.clone());
        }

        sq
    }

    async fn call_llm(&self, query: &str) -> StructuredQuery {
        let request = LlmRequest {
            model: self.model.name().to_string(),
            contents: vec![
                Content::new("system").with_text(UNDERSTAND_PROMPT),
                Content::new("user").with_text(query),
            ],
            tools: std::collections::HashMap::new(),
            config: Some(GenerateContentConfig {
                temperature: Some(0.0),
                max_output_tokens: Some(256),
                ..Default::default()
            }),
            previous_response_id: None,
        };

        match self.model.generate_content(request, false).await {
            Ok(mut stream) => {
                // 消费 stream，收集所有文本
                let mut text = String::new();
                while let Some(chunk_result) = stream.next().await {
                    match chunk_result {
                        Ok(response) => {
                            if let Some(c) = &response.content {
                                for p in &c.parts {
                                    if let Some(t) = p.text() {
                                        text.push_str(t);
                                    }
                                }
                            }
                            if response.turn_complete {
                                break;
                            }
                        }
                        Err(e) => {
                            tracing::warn!("[QueryUnderstanding] stream 错误: {}", e);
                            break;
                        }
                    }
                }

                // 尝试从文本中提取 JSON
                let json_str = extract_json(&text);
                match serde_json::from_str::<StructuredQuery>(json_str) {
                    Ok(sq) => sq,
                    Err(_) => {
                        tracing::warn!("[QueryUnderstanding] LLM 输出解析失败: {}", text);
                        StructuredQuery::default()
                    }
                }
            }
            Err(e) => {
                tracing::warn!("[QueryUnderstanding] LLM 调用失败: {}", e);
                StructuredQuery::default()
            }
        }
    }
}

/// 从可能包含 markdown 代码块的文本中提取 JSON
///
/// LLM 输出可能被 ```json ... ``` 包裹，此函数找到第一个 `{` 和最后一个 `}` 之间的内容。
/// 如果没有找到花括号则返回原始文本（trim 后）。
fn extract_json(text: &str) -> &str {
    let trimmed = text.trim();
    if let Some(start) = trimmed.find('{')
        && let Some(end) = trimmed.rfind('}')
    {
        return &trimmed[start..=end];
    }
    trimmed
}
