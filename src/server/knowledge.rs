//! 知识库管理 API — 多 provider（FAQ 学习 + 实例文档操作）。
//!
//! - FAQ 学习（kbLearn/kbLearnRegenerate/kbLearnCommit）走 provider（commit_faqs → upload_to_instance），
//!   写入哪个实例由 `instance_id` 决定（不传则取第一个启用实例）。
//! - 文档操作（kbInstanceUpload/kbInstanceDocuments/kbInstanceSegments/kbInstanceDeleteDocument）
//!   按 `kb_instance_id` 路由到对应 provider。
//!
//! 旧的 dify 直连接口（kbUpload/kbDocuments/kbFeedback/deleteDocument/kbDocumentSegments）已废弃移除
//! （前端统一走 instance 接口）。

use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

use super::AppState;
use super::response;
use super::response::code;

/// 解析知识萃取（FAQ 学习）使用的 LLM。
///
/// 模型解析：DB 供应商存储为唯一数据源；未初始化或无可用模型时返回业务错误。
/// 优先使用前端传来的 `model_id`；返回 (模型实例, 模型名) 供 `KnowledgeManager` 构造请求。
fn resolve_kb_model(
    state: &AppState,
    model_id: Option<&str>,
    session_id: &str,
) -> Result<(Arc<dyn adk_rust::Llm>, String), String> {
    let trimmed = model_id.map(str::trim).filter(|s| !s.is_empty());
    let store = state
        .model_provider_store
        .as_ref()
        .filter(|s| s.has_models())
        .ok_or_else(|| {
            "模型供应商存储未初始化或无可用模型，请在模型供应商管理中配置模型".to_string()
        })?;
    let resolved = store.resolve_model(trimmed).map_err(|e| e.to_string())?;
    tracing::info!(
        "[kb_learn] session={} 使用模型 id={} name={} model={}",
        session_id,
        resolved.id,
        resolved.name,
        resolved.model
    );
    let model = crate::llm::make_model_from_resolved(&resolved).map_err(|e| e.to_string())?;
    Ok((model, resolved.model))
}

/// 解析 FAQ 学习的目标知识库实例：显式传则用，否则取第一个启用实例。
async fn resolve_kb_instance(state: &AppState, id: Option<&str>) -> Result<String, Value> {
    match id.map(str::trim).filter(|s| !s.is_empty()) {
        Some(id) => Ok(id.to_string()),
        None => match state.knowledge_manager.first_enabled_instance_id().await {
            Ok(id) => Ok(id),
            Err(e) => Err(response::err(code::BUSINESS, e.to_string())),
        },
    }
}

// ========================================================================
//  FAQ 学习（走 provider：候选生成 → 前端审查 → 提交写入）
// ========================================================================

#[derive(Debug, Deserialize)]
pub struct KbLearnRequest {
    pub session_id: String,
    pub brand: String,
    pub dev_type: String,
    /// 设备型号（可选，如 S5300）
    #[serde(default)]
    pub model: String,
    /// 写入的知识库实例（不传则用第一个启用的实例）
    #[serde(default)]
    pub instance_id: Option<String>,
    #[serde(default)]
    pub model_id: Option<String>,
}

/// 读取指定会话的完整历史，拼接为 `角色: 文本` 形式的对话字符串
///
/// 供 FAQ 生成 / 重生成共用。失败返回业务错误消息。
async fn read_session_conversation(
    state: &AppState,
    user_id: &str,
    session_id: &str,
) -> Result<String, String> {
    let get_req = adk_rust::session::GetRequest {
        app_name: "cortex-agent".to_string(),
        user_id: user_id.to_string(),
        session_id: session_id.to_string(),
        num_recent_events: None,
        after: None,
    };

    let session = state
        .adk_session_service
        .get(get_req)
        .await
        .map_err(|e| format!("读取会话历史失败: {}", e))?;

    let events = session.events();
    let mut msgs = Vec::new();
    for i in 0..events.len() {
        if let Some(event) = events.at(i)
            && let Some(content) = &event.llm_response.content
        {
            for part in &content.parts {
                if let adk_rust::Part::Text { text } = part
                    && !text.is_empty()
                {
                    let role = if event.author == "user" {
                        "用户"
                    } else {
                        "助手"
                    };
                    msgs.push(format!("{}: {}", role, text));
                }
            }
        }
    }
    Ok(msgs.join("\n\n"))
}

/// 第一阶段：从会话生成多组 FAQ 候选返回前端审查（不写入知识库）
pub async fn kb_learn(state: &AppState, user_id: &str, input: KbLearnRequest) -> Value {
    let conversation = match read_session_conversation(state, user_id, &input.session_id).await {
        Ok(c) => c,
        Err(e) => return response::err(code::DATABASE, e),
    };

    if conversation.trim().is_empty() {
        return response::err(code::BUSINESS, "会话历史为空");
    }

    let (model, model_name) =
        match resolve_kb_model(state, input.model_id.as_deref(), input.session_id.as_str()) {
            Ok(v) => v,
            Err(e) => return response::err(code::LLM, e),
        };

    let instance_id = match resolve_kb_instance(state, input.instance_id.as_deref()).await {
        Ok(id) => id,
        Err(v) => return v,
    };
    match state
        .knowledge_manager
        .generate_candidates(
            &instance_id,
            &input.brand,
            &input.dev_type,
            &input.model,
            &conversation,
            model,
            &model_name,
        )
        .await
    {
        Ok(candidates) => response::ok(json!({
            "count": candidates.len(),
            "candidates": candidates,
            "message": format!("已生成 {} 组 FAQ 候选，请审查后勾选上传", candidates.len()),
        })),
        Err(e) => response::err(code::LLM, e.to_string()),
    }
}

/// 第一阶段（变体）：对指定主题重新生成 FAQ 候选（用户对某条不满意时循环调用）
#[derive(Debug, Deserialize)]
pub struct KbRegenerateRequest {
    pub session_id: String,
    pub brand: String,
    pub dev_type: String,
    /// 设备型号（可选，如 S5300）
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub instance_id: Option<String>,
    /// 指定重新生成的主题标题；为空则全部重新生成
    pub target_title: Option<String>,
    /// 用户补充的修改要求
    pub feedback: Option<String>,
    #[serde(default)]
    pub model_id: Option<String>,
}

pub async fn kb_learn_regenerate(
    state: &AppState,
    user_id: &str,
    input: KbRegenerateRequest,
) -> Value {
    let conversation = match read_session_conversation(state, user_id, &input.session_id).await {
        Ok(c) => c,
        Err(e) => return response::err(code::DATABASE, e),
    };

    if conversation.trim().is_empty() {
        return response::err(code::BUSINESS, "会话历史为空");
    }

    let target = input
        .target_title
        .as_deref()
        .filter(|s| !s.trim().is_empty());
    let feedback = input.feedback.as_deref().filter(|s| !s.trim().is_empty());

    let (model, model_name) =
        match resolve_kb_model(state, input.model_id.as_deref(), input.session_id.as_str()) {
            Ok(v) => v,
            Err(e) => return response::err(code::LLM, e),
        };

    let instance_id = match resolve_kb_instance(state, input.instance_id.as_deref()).await {
        Ok(id) => id,
        Err(v) => return v,
    };
    match state
        .knowledge_manager
        .regenerate_candidates(
            &instance_id,
            &input.brand,
            &input.dev_type,
            &input.model,
            &conversation,
            target,
            feedback,
            model,
            &model_name,
        )
        .await
    {
        Ok(candidates) => response::ok(json!({
            "count": candidates.len(),
            "candidates": candidates,
            "message": format!("已重新生成 {} 组 FAQ 候选", candidates.len()),
        })),
        Err(e) => response::err(code::LLM, e.to_string()),
    }
}

/// 第二阶段：提交用户勾选的 FAQ 候选，写入指定知识库实例
#[derive(Debug, Deserialize)]
pub struct KbFaqItem {
    pub title: String,
    pub content: String,
}

#[derive(Debug, Deserialize)]
pub struct KbCommitRequest {
    #[serde(default)]
    pub instance_id: Option<String>,
    /// 可选属性：厂商（写入 FAQ 文档 metadata，供检索过滤）
    #[serde(default)]
    pub brand: Option<String>,
    /// 可选属性：设备类型（写入 FAQ 文档 metadata，供检索过滤）
    #[serde(default)]
    pub dev_type: Option<String>,
    /// 可选属性：设备型号（写入 FAQ 文档 metadata，供检索过滤）
    #[serde(default)]
    pub model: Option<String>,
    pub items: Vec<KbFaqItem>,
}

pub async fn kb_learn_commit(state: &AppState, input: KbCommitRequest) -> Value {
    if input.items.is_empty() {
        return response::err(code::BUSINESS, "未勾选任何 FAQ");
    }

    let candidates: Vec<crate::domain::knowledge::FaqCandidate> = input
        .items
        .iter()
        .map(|i| crate::domain::knowledge::FaqCandidate {
            title: i.title.clone(),
            content: i.content.clone(),
            duplicate: false,
            char_count: i.content.chars().count(),
        })
        .collect();

    let instance_id = match resolve_kb_instance(state, input.instance_id.as_deref()).await {
        Ok(id) => id,
        Err(v) => return v,
    };
    match state
        .knowledge_manager
        .commit_faqs(
            &instance_id,
            input.brand.as_deref().unwrap_or(""),
            input.dev_type.as_deref().unwrap_or(""),
            input.model.as_deref().unwrap_or(""),
            &candidates,
        )
        .await
    {
        Ok(count) => response::ok(json!({
            "count": count,
            "message": format!("已写入 {} 条 FAQ 到知识库", count),
        })),
        Err(e) => response::err(code::NETWORK, e.to_string()),
    }
}

// ========================================================================
//  多 provider 文档操作（按 kb_instance_id 路由到对应 provider）
// ========================================================================

#[derive(Debug, Deserialize)]
pub struct KbInstanceDocUploadRequest {
    pub instance_id: String,
    pub title: String,
    pub content: String,
    pub user_role: Option<String>,
    /// 可选属性：厂商（空=不设 metadata）
    #[serde(default)]
    pub brand: Option<String>,
    /// 可选属性：设备类型（空=不设 metadata）
    #[serde(default)]
    pub dev_type: Option<String>,
    /// 可选属性：设备型号，如 S5300（空=不设 metadata）
    #[serde(default)]
    pub model: Option<String>,
}

pub async fn kb_instance_upload(state: &AppState, input: KbInstanceDocUploadRequest) -> Value {
    let inp = crate::domain::knowledge::backend::KbDocInput {
        brand: input.brand.unwrap_or_default(),
        dev_type: input.dev_type.unwrap_or_default(),
        model: input.model.unwrap_or_default(),
        firmware_ver: String::new(),
        title: input.title,
        content: input.content,
        user_role: input.user_role.unwrap_or_else(|| "admin".to_string()),
    };
    match state
        .knowledge_manager
        .upload_to_instance(&input.instance_id, inp)
        .await
    {
        Ok(id) => response::ok(json!({ "doc_id": id, "message": "上传成功" })),
        Err(e) => response::err(code::NETWORK, e.to_string()),
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct KbInstanceDocsQuery {
    pub instance_id: String,
    pub keyword: Option<String>,
    pub page: Option<u32>,
    pub page_size: Option<u32>,
}

pub async fn kb_instance_documents(state: &AppState, params: KbInstanceDocsQuery) -> Value {
    let page = params.page.unwrap_or(1).max(1);
    let limit = params.page_size.unwrap_or(20).clamp(1, 100);
    let f = crate::domain::knowledge::backend::KbListFilter {
        page,
        limit,
        brand: None,
        dev_type: None,
        keyword: params.keyword,
    };
    match state
        .knowledge_manager
        .list_instance(&params.instance_id, f)
        .await
    {
        Ok(res) => response::ok(json!({
            "total": res.total,
            "page": res.page,
            "page_size": res.limit,
            "documents": res.data,
        })),
        Err(e) => response::err(code::NETWORK, e.to_string()),
    }
}

pub async fn kb_instance_segments(state: &AppState, instance_id: &str, doc_id: &str) -> Value {
    match state
        .knowledge_manager
        .segments_instance(instance_id, doc_id)
        .await
    {
        Ok(segs) => response::ok(json!({ "segments": segs })),
        Err(e) => response::err(code::NETWORK, e.to_string()),
    }
}

pub async fn kb_instance_delete_document(
    state: &AppState,
    instance_id: &str,
    doc_id: &str,
) -> Value {
    match state
        .knowledge_manager
        .delete_instance(instance_id, doc_id)
        .await
    {
        Ok(()) => response::ok(json!({ "deleted": true })),
        Err(e) => response::err(code::NETWORK, e.to_string()),
    }
}
