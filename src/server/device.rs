//! 设备检索 API 模块 — 知识库语义检索入口
//!
//! 通过 LLM 查询理解提取结构化条件后调用知识库进行语义检索，
//! 返回匹配的设备运维知识文档列表。原 `POST /api/device/search`，
//! 现已迁移至 GraphQL `deviceSearch(input: JSON!)`。

use serde::Deserialize;
use serde_json::json;

use super::AppState;
use super::response;

// ========================================================================
//  设备检索
// ========================================================================

#[derive(Debug, Deserialize)]
pub struct DeviceSearchRequest {
    pub query: String,
    pub brand: Option<String>,
    pub dev_type: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
}

pub async fn device_search(state: &AppState, input: DeviceSearchRequest) -> serde_json::Value {
    // LLM 查询理解：提取厂商/设备类型/关键词（显式参数优先）
    let sq = state.query_understanding.understand(&input.query).await;
    let brand = input.brand.or(sq.brand);
    let dev_type = input.dev_type.or(sq.dev_type);
    let model = input.model.or(sq.model);
    // 用原始查询 + LLM 关键词拼接，确保不丢信息
    let search_query = if sq.keywords.is_empty() {
        input.query.clone()
    } else {
        format!("{} {}", input.query, sq.keywords.join(" "))
    };

    tracing::info!(
        "[search] query=\"{}\" → brand={:?}, dev_type={:?}, search_query=\"{}\"",
        input.query,
        brand,
        dev_type,
        search_query
    );

    let instance_id = match state.knowledge_manager.first_enabled_instance_id().await {
        Ok(id) => id,
        Err(e) => return response::err(response::code::BUSINESS, e.to_string()),
    };
    let kbq = crate::domain::knowledge::backend::KbQuery {
        query: search_query,
        brand,
        dev_type,
        model,
        topk: None,
    };
    match state
        .knowledge_manager
        .search_instance(&instance_id, kbq)
        .await
    {
        Ok(results) => response::ok(json!({ "results": results })),
        Err(e) => response::err(response::code::NETWORK, e.to_string()),
    }
}
