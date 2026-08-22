//! 设备命令工具模块 — 知识库检索与设备目录查询
//!
//! 提供两个 FunctionTool 供 DeviceAgent 使用：
//!
//! - **search_kb**：知识库语义检索
//!   - 先调用 LLM 查询理解提取厂商/设备类型/关键词
//!   - 显式参数优先于 LLM 提取结果
//!   - 返回匹配的文档列表（标题、内容、厂商、设备类型等）
//!
//! - **query_device_catalog**：设备目录模糊匹配
//!   - 不填关键词时返回全部厂商和设备类型
//!   - 填关键词时进行模糊匹配
//!   - 多个匹配时提示存在歧义需用户确认

use adk_rust::tool::FunctionTool;
use adk_rust::{
    ToolContext,
    serde_json::{Value, json},
};
use schemars::JsonSchema;
use serde::Serialize;
use std::sync::Arc;

use crate::agent::query_understanding::QueryUnderstandingService;
use crate::domain::device_catalog::CatalogCache;
use crate::domain::knowledge::KnowledgeManager;

// ============ 参数 Schema 定义 ============

/// search_kb 工具参数
#[derive(Debug, Serialize, JsonSchema)]
pub struct SearchKbParams {
    /// 搜索关键词，如"静态路由配置"、"OSPF区域配置"
    pub query: String,
    /// 设备厂商，如"H3C"、"Huawei"（可选）
    #[serde(default)]
    pub brand: Option<String>,
    /// 设备类型，如"router"、"switch"（可选）
    #[serde(default)]
    pub dev_type: Option<String>,
    /// 设备型号，如"S5300"（可选）
    #[serde(default)]
    pub model: Option<String>,
    /// 业务ID，默认"default"（可选）
    #[serde(default)]
    pub biz_id: Option<String>,
}

/// query_device_catalog 工具参数
#[derive(Debug, Serialize, JsonSchema)]
pub struct QueryCatalogParams {
    /// 模糊匹配关键词，如"H3C"、"路由器"。不填则返回全部目录
    #[serde(default)]
    pub keyword: Option<String>,
    /// 限定类别：'brand' 或 'dev_type'。不填则同时匹配厂商和设备类型
    #[serde(default)]
    pub category: Option<String>,
}

// ============ 工具创建函数 ============

/// 创建知识库检索工具（`search_kb`）
///
/// 工具执行流程：
/// 1. 调用 `QueryUnderstandingService` 从查询中提取厂商/设备类型/关键词
/// 2. 显式参数优先于 LLM 提取结果
/// 3. 将原始查询 + LLM 关键词拼接，确保不丢信息
/// 4. 调用 `KnowledgeManager::search` 进行检索
pub fn create_search_tool(
    knowledge_manager: Arc<KnowledgeManager>,
    query_understanding: Arc<QueryUnderstandingService>,
    instance_id: Option<String>,
) -> FunctionTool {
    let km1 = knowledge_manager.clone();
    let qu1 = query_understanding.clone();
    let inst1 = instance_id;
    FunctionTool::new(
        "search_kb",
        "搜索网络设备运维知识库。当用户询问任何设备配置命令时必须先调用此工具。",
        move |_ctx: Arc<dyn ToolContext>, args: Value| {
            let km = km1.clone();
            let qu = qu1.clone();
            let inst = inst1.clone();
            async move {
                let query = args["query"].as_str().unwrap_or("");
                let brand = args["brand"].as_str().map(|s| s.to_string());
                let dev_type = args["dev_type"].as_str().map(|s| s.to_string());
                let model = args["model"].as_str().map(|s| s.to_string());

                if query.is_empty() {
                    return Ok(json!({ "ok": false, "message": "查询内容不能为空" }));
                }

                // LLM 查询理解：提取厂商/设备类型/关键词（显式参数优先）
                let sq = qu.understand(query).await;
                let final_brand = brand.or(sq.brand.clone());
                let final_dev_type = dev_type.or(sq.dev_type.clone());
                let final_model = model.or(sq.model.clone());
                // 用原始查询 + LLM 关键词拼接，确保不丢信息
                let search_query = if sq.keywords.is_empty() {
                    query.to_string()
                } else {
                    format!("{} {}", query, sq.keywords.join(" "))
                };

                tracing::info!("[search_kb] query=\"{}\" → brand={:?}, dev_type={:?}, model={:?}, keywords={:?}",
                    query, final_brand, final_dev_type, final_model, sq.keywords);

                // 检索：走助手绑定的实例；未绑（builtin 助手未配置知识库）则提示去配置，不做兜底
                let id = match &inst {
                    Some(id) => id.as_str(),
                    None => {
                        return Ok(json!({
                            "ok": false,
                            "message": "助手未绑定知识库实例，请在助手设置中配置知识库",
                            "documents": []
                        }));
                    }
                };
                let sanitize = |s: &str| -> String {
                    s.replace('<', "＜").replace('>', "＞")
                };
                let kbq = crate::domain::knowledge::backend::KbQuery {
                    query: search_query.clone(),
                    brand: final_brand.clone(),
                    dev_type: final_dev_type.clone(),
                    model: final_model.clone(),
                    topk: None,
                };
                let docs: Vec<Value> = match km.search_instance(id, kbq).await {
                    Ok(rows) => rows
                        .iter()
                        .map(|d| {
                            json!({
                                "title": sanitize(&d.title),
                                "content": sanitize(&d.content),
                                "brand": d.brand,
                                "dev_type": d.dev_type,
                                "model": d.model,
                                "source": d.source,
                            })
                        })
                        .collect(),
                    Err(e) => {
                        return Ok(json!({ "ok": false, "message": e.to_string(), "documents": [] }));
                    }
                };

                if docs.is_empty() {
                    Ok(json!({ "ok": true, "count": 0, "source": "knowledge_base", "message": "知识库无匹配，请使用自身知识回答", "documents": [] }))
                } else {
                    Ok(json!({ "ok": true, "count": docs.len(), "source": "knowledge_base", "documents": docs }))
                }
            }
        },
    )
    .with_parameters_schema::<SearchKbParams>()
}

/// 设备目录查询工具 — 供 IntentAgent 语义匹配厂商和设备类型
pub fn create_catalog_tool(catalog: Arc<CatalogCache>) -> FunctionTool {
    let cat = catalog.clone();
    FunctionTool::new(
        "query_device_catalog",
        "查询系统设备目录，获取所有支持的厂商和设备类型列表，或模糊匹配用户输入。当需要确认用户提到的厂商/设备类型是否合法、或存在歧义需要消解时调用。",
        move |_ctx: Arc<dyn ToolContext>, args: Value| {
            let cat = cat.clone();
            async move {
                let keyword = args["keyword"].as_str().unwrap_or("");
                let category = args["category"].as_str().unwrap_or("");

                if keyword.is_empty() {
                    // 返回全部目录
                    let catalog_json = cat.to_json().await;
                    let brand_count = catalog_json["brands"].as_array().map(|a| a.len()).unwrap_or(0);
                    let type_count = catalog_json["dev_types"].as_array().map(|a| a.len()).unwrap_or(0);
                    return Ok(json!({
                        "ok": true,
                        "message": format!("共 {} 个厂商, {} 个设备类型", brand_count, type_count),
                        "catalog": catalog_json,
                    }));
                }

                // 模糊匹配
                let mut result = json!({});
                if category.is_empty() || category == "brand" {
                    let matched = cat.match_brand(keyword).await;
                    result["brands"] = json!(matched.iter().map(|b| json!({
                        "id": b.id, "name_ch": b.name_ch, "name_en": b.name_en,
                    })).collect::<Vec<_>>());
                }
                if category.is_empty() || category == "dev_type" {
                    let matched = cat.match_dev_type(keyword).await;
                    result["dev_types"] = json!(matched.iter().map(|t| json!({
                        "id": t.id, "name_ch": t.name_ch, "name_en": t.name_en,
                    })).collect::<Vec<_>>());
                }

                let brand_count = result["brands"].as_array().map(|a| a.len()).unwrap_or(0);
                let type_count = result["dev_types"].as_array().map(|a| a.len()).unwrap_or(0);

                Ok(json!({
                    "ok": true,
                    "keyword": keyword,
                    "matched_brands": brand_count,
                    "matched_types": type_count,
                    "result": result,
                    "hint": if brand_count > 1 || type_count > 1 {
                        "存在多个匹配，请让用户确认具体是哪一个"
                    } else if brand_count == 0 && type_count == 0 {
                        "未匹配到任何厂商或设备类型，请让用户提供更准确的名称"
                    } else {
                        "匹配明确，可以继续"
                    },
                }))
            }
        },
    )
    .with_parameters_schema::<QueryCatalogParams>()
}
