//! 自定义助手 GraphQL resolver 层（CRUD + 复制 + 广场 + 分享 + 导入导出）。
//!
//! 助手功能已从 REST 迁移到 GraphQL，不再注册 REST 路由。
//! 所有 handler 签名统一为 `(&AppState, ...) -> Value`，
//! 返回标准信封 `{ code, message, data }`，由 graphql.rs 的 Query/Mutation 字段调用。
//!
//! 保留的 REST/SSE 路由：`POST /api/run_sse`、`POST /api/brainstorm/generate`、
//! `GET /api/health`、`GET /api/v1/monitor/health` 不受影响。

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::domain::assistant::{
    AgentType, Assistant, AssistantKind, CustomAssistantInput, Visibility,
};
use crate::server::AppState;
use crate::server::response::{self, code};
use crate::tools::registry::sanitize_custom_tools;

// ===========================================================================
// DTO
// ===========================================================================

/// 创建 / 更新自定义助手的输入（PUT 为整体替换语义，复用同结构）
#[derive(Debug, Clone, Deserialize)]
pub struct WriteAssistantRequest {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub avatar: String,
    #[serde(default)]
    pub system_prompt: String,
    #[serde(default)]
    pub model_id: String,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub max_tokens: Option<i32>,
    /// 思考级别：low/medium/high，None=不发（走模型默认）
    #[serde(default)]
    pub thinking_level: Option<String>,
    #[serde(default)]
    pub enabled_tools: Vec<String>,
    #[serde(default)]
    pub knowledge_enabled: bool,
    /// 绑定的知识库实例 id（多 provider；None=不绑定）
    #[serde(default)]
    pub kb_instance_id: Option<String>,
    /// 已启用的 MCP Server id 列表
    #[serde(default)]
    pub enabled_mcps: Vec<String>,
    #[serde(default)]
    pub greeting: String,
    #[serde(default = "default_visibility_private")]
    pub visibility: i16,
}

fn default_visibility_private() -> i16 {
    0
}

impl WriteAssistantRequest {
    fn to_input(&self) -> Result<CustomAssistantInput, (i32, String)> {
        let name = self.name.trim().to_string();
        if name.is_empty() {
            return Err((code::INVALID_PARAMS, "助手名称不能为空".into()));
        }
        if name.chars().count() > 32 {
            return Err((code::INVALID_PARAMS, "助手名称不能超过 32 字".into()));
        }
        if self.system_prompt.chars().count() > 8000 {
            return Err((code::INVALID_PARAMS, "系统提示词不能超过 8000 字".into()));
        }
        let visibility = Visibility::try_from_i16(self.visibility).ok_or_else(|| {
            (
                code::INVALID_PARAMS,
                format!("非法的 visibility 值: {}", self.visibility),
            )
        })?;
        let temperature = self.temperature.map(|t| t.clamp(0.0, 2.0));
        let top_p = self.top_p.map(|p| p.clamp(0.0, 1.0));
        let max_tokens = self.max_tokens.map(|m| m.clamp(16384, 32768));
        let thinking_level = match self.thinking_level.as_deref() {
            None | Some("") => None,
            Some(v) => match v {
                "low" | "medium" | "high" | "xhigh" | "max" => Some(v.to_string()),
                _ => {
                    return Err((
                        code::INVALID_PARAMS,
                        format!("非法的 thinking_level 值: {v}"),
                    ));
                }
            },
        };
        Ok(CustomAssistantInput {
            name,
            description: self.description.trim().to_string(),
            avatar: self.avatar.trim().to_string(),
            system_prompt: self.system_prompt.clone(),
            model_id: self.model_id.trim().to_string(),
            temperature,
            top_p,
            max_tokens,
            thinking_level,
            enabled_tools: sanitize_custom_tools(&self.enabled_tools),
            knowledge_enabled: self.knowledge_enabled,
            kb_instance_id: self.kb_instance_id.clone(),
            enabled_mcps: self.enabled_mcps.clone(),
            greeting: self.greeting.clone(),
            visibility,
        })
    }
}

/// 助手响应 DTO（含完整字段）
#[derive(Debug, Clone, Serialize)]
pub struct AssistantDto {
    pub id: String,
    pub name: String,
    pub description: String,
    pub avatar: String,
    pub kind: i16,
    pub agent_type: i16,
    pub agent_type_key: &'static str,
    pub system_prompt: String,
    pub model_id: String,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub max_tokens: Option<i32>,
    pub thinking_level: Option<String>,
    pub enabled_tools: Vec<String>,
    pub knowledge_enabled: bool,
    pub kb_instance_id: Option<String>,
    pub enabled_mcps: Vec<String>,
    pub greeting: String,
    pub share_token: String,
    pub fork_count: i32,
    pub creator: String,
    pub visibility: i16,
    pub sort_order: i32,
}

impl From<Assistant> for AssistantDto {
    fn from(a: Assistant) -> Self {
        Self {
            id: a.id,
            name: a.name,
            description: a.description,
            avatar: a.avatar,
            kind: a.kind.as_i16(),
            agent_type: a.agent_type.as_i16(),
            agent_type_key: a.agent_type.dispatch_key(),
            system_prompt: a.system_prompt,
            model_id: a.model_id,
            temperature: a.temperature,
            top_p: a.top_p,
            max_tokens: a.max_tokens,
            thinking_level: a.thinking_level,
            enabled_tools: a.enabled_tools,
            knowledge_enabled: a.knowledge_enabled,
            kb_instance_id: a.kb_instance_id,
            enabled_mcps: a.enabled_mcps,
            greeting: a.greeting,
            share_token: a.share_token,
            fork_count: a.fork_count,
            creator: a.creator,
            visibility: a.visibility.as_i16(),
            sort_order: a.sort_order,
        }
    }
}

/// 广场/分享查询返回的脱敏 DTO（不含 system_prompt / share_token / creator）
#[derive(Debug, Clone, Serialize)]
pub struct AssistantPublicDto {
    pub id: String,
    pub name: String,
    pub description: String,
    pub avatar: String,
    pub kind: i16,
    pub agent_type: i16,
    pub agent_type_key: &'static str,
    pub greeting: String,
    pub enabled_tools: Vec<String>,
    pub fork_count: i32,
}

impl From<Assistant> for AssistantPublicDto {
    fn from(a: Assistant) -> Self {
        Self {
            id: a.id,
            name: a.name,
            description: a.description,
            avatar: a.avatar,
            kind: a.kind.as_i16(),
            agent_type: a.agent_type.as_i16(),
            agent_type_key: a.agent_type.dispatch_key(),
            greeting: a.greeting,
            enabled_tools: a.enabled_tools,
            fork_count: a.fork_count,
        }
    }
}

// ===========================================================================
// helpers
// ===========================================================================

fn store_err() -> Value {
    response::err(code::DATABASE, "助手存储不可用（数据库未启用）")
}

fn business_err_obj(code: i32, msg: impl Into<String>) -> Value {
    response::err(code, msg)
}

fn current_creator(_state: &AppState) -> &'static str {
    "local"
}

fn assert_writable(
    a: &crate::domain::assistant::Assistant,
    expected_creator: &str,
) -> Result<(), (i32, String)> {
    if a.kind != AssistantKind::Custom {
        return Err((code::BUSINESS, "内置助手不可修改".into()));
    }
    if a.creator != expected_creator {
        return Err((code::BUSINESS, "无权操作他人创建的助手".into()));
    }
    Ok(())
}

fn assert_exportable(
    a: &crate::domain::assistant::Assistant,
    expected_creator: &str,
) -> Result<(), (i32, String)> {
    if a.creator == expected_creator {
        return Ok(());
    }
    let public = matches!(a.visibility, Visibility::Shared | Visibility::Builtin)
        || a.kind == AssistantKind::Builtin;
    if public {
        Ok(())
    } else {
        Err((code::NOT_FOUND, "助手不存在或未公开".into()))
    }
}

fn get_store(
    state: &AppState,
) -> Result<&Arc<crate::domain::assistant::store::AssistantStore>, Value> {
    state.assistant_store.as_ref().ok_or_else(store_err)
}

// ===========================================================================
// Query resolvers
// ===========================================================================

/// 列出全部助手（内置 + 自定义）
pub async fn list_assistants(state: &AppState) -> Value {
    let store = match get_store(state) {
        Ok(s) => s,
        Err(v) => return v,
    };
    match store.list_all().await {
        Ok(list) => {
            let dtos: Vec<AssistantDto> = list.into_iter().map(AssistantDto::from).collect();
            response::ok(json!({ "assistants": dtos }))
        }
        Err(e) => {
            tracing::error!(target: "assistant", "list_assistants 失败: {e}");
            response::err(code::DATABASE, format!("查询失败: {e}"))
        }
    }
}

/// 获取单个助手详情
pub async fn get_assistant(state: &AppState, id: &str) -> Value {
    let store = match get_store(state) {
        Ok(s) => s,
        Err(v) => return v,
    };
    match store.get(id).await {
        Ok(Some(a)) => response::ok(json!({ "assistant": AssistantDto::from(a) })),
        Ok(None) => response::err(code::NOT_FOUND, "助手不存在"),
        Err(e) => {
            tracing::error!(target: "assistant", "get_assistant {id} 失败: {e}");
            response::err(code::DATABASE, format!("查询失败: {e}"))
        }
    }
}

/// 广场列表（公开助手，脱敏）
pub async fn list_explore(state: &AppState) -> Value {
    let store = match get_store(state) {
        Ok(s) => s,
        Err(v) => return v,
    };
    match store.list_public().await {
        Ok(list) => {
            let dtos: Vec<AssistantPublicDto> =
                list.into_iter().map(AssistantPublicDto::from).collect();
            response::ok(json!({ "assistants": dtos }))
        }
        Err(e) => {
            tracing::error!(target: "assistant", "list_explore 失败: {e}");
            response::err(code::DATABASE, format!("查询失败: {e}"))
        }
    }
}

/// 按分享口令查询助手（公开，脱敏）
pub async fn get_by_token(state: &AppState, token: &str) -> Value {
    let store = match get_store(state) {
        Ok(s) => s,
        Err(v) => return v,
    };
    match store.get_by_token(token).await {
        Ok(Some(a)) => response::ok(json!({ "assistant": AssistantPublicDto::from(a) })),
        Ok(None) => response::err(code::NOT_FOUND, "口令无效或已失效"),
        Err(e) => {
            tracing::error!(target: "assistant", "get_by_token 失败: {e}");
            response::err(code::DATABASE, format!("查询失败: {e}"))
        }
    }
}

/// 列出可勾选工具
pub async fn list_tools(_state: &AppState) -> Value {
    let tools: Vec<Value> = crate::tools::registry::custom_options()
        .iter()
        .map(|info| {
            json!({
                "key": info.key,
                "name": info.name,
                "description": info.description,
            })
        })
        .collect();
    response::ok(json!({ "tools": tools }))
}

// ===========================================================================
// Mutation resolvers
// ===========================================================================

/// 创建自定义助手
pub async fn create_assistant(state: &AppState, input: &Value) -> Value {
    let store = match get_store(state) {
        Ok(s) => s,
        Err(v) => return v,
    };
    let req: WriteAssistantRequest = match serde_json::from_value(input.clone()) {
        Ok(r) => r,
        Err(e) => return response::err(code::PARSE_ERROR, format!("参数解析失败: {e}")),
    };
    let input_data = match req.to_input() {
        Ok(i) => i,
        Err((c, m)) => return business_err_obj(c, m),
    };
    let creator = current_creator(state);
    match store.create_custom(&input_data, creator).await {
        Ok(id) => {
            tracing::info!(target: "assistant", "create_assistant name={} → id={}", input_data.name, id);
            response::ok(json!({ "id": id }))
        }
        Err(e) => {
            tracing::error!(target: "assistant", "create_assistant 失败: {e}");
            response::err(code::DATABASE, format!("创建失败: {e}"))
        }
    }
}

/// 更新自定义助手
pub async fn update_assistant(state: &AppState, id: &str, input_val: &Value) -> Value {
    let store = match get_store(state) {
        Ok(s) => s,
        Err(v) => return v,
    };
    let req: WriteAssistantRequest = match serde_json::from_value(input_val.clone()) {
        Ok(r) => r,
        Err(e) => return response::err(code::PARSE_ERROR, format!("参数解析失败: {e}")),
    };
    let input_data = match req.to_input() {
        Ok(i) => i,
        Err((c, m)) => return business_err_obj(c, m),
    };
    let creator = current_creator(state);
    match store.get(id).await {
        Ok(Some(a)) => {
            if let Err((c, m)) = assert_writable(&a, creator) {
                return business_err_obj(c, m);
            }
        }
        Ok(None) => return response::err(code::NOT_FOUND, "助手不存在"),
        Err(e) => {
            tracing::error!(target: "assistant", "update_assistant 查询 {id} 失败: {e}");
            return response::err(code::DATABASE, format!("查询失败: {e}"));
        }
    }
    match store.update_custom(id, &input_data).await {
        Ok(true) => response::ok(json!({ "updated": true })),
        Ok(false) => response::err(code::NOT_FOUND, "助手不存在或为内置（不可修改）"),
        Err(e) => {
            tracing::error!(target: "assistant", "update_assistant {id} 失败: {e}");
            response::err(code::DATABASE, format!("更新失败: {e}"))
        }
    }
}

/// 删除自定义助手（两步合一）
///
/// - `force=false`（默认）：dry-run 预检，返回引用影响清单，不删除
/// - `force=true`：单个事务内级联清理所有引用（保留引用方主体），再删除助手
pub async fn delete_assistant(state: &AppState, id: &str, force: bool) -> Value {
    let store = match get_store(state) {
        Ok(s) => s,
        Err(v) => return v,
    };
    let creator = current_creator(state);
    // 权限/存在性校验：无论是否 force，都先确认助手存在且可写
    match store.get(id).await {
        Ok(Some(a)) => {
            if let Err((c, m)) = assert_writable(&a, creator) {
                return business_err_obj(c, m);
            }
        }
        Ok(None) => return response::err(code::NOT_FOUND, "助手不存在"),
        Err(e) => {
            tracing::error!(target: "assistant", "delete_assistant 查询 {id} 失败: {e}");
            return response::err(code::DATABASE, format!("查询失败: {e}"));
        }
    }

    if !force {
        // 预检：返回影响清单，不删除
        match store.impact_of_delete(id).await {
            Ok(impact) => response::ok(json!({
                "deleted": false,
                "impact": {
                    "sessions": impact.sessions,
                    "memories": impact.memories,
                    "memory_proposals": impact.memory_proposals,
                },
                "summary": summarize_assistant_impact(&impact),
            })),
            Err(e) => {
                tracing::error!(target: "assistant", "delete_assistant 预检 {id} 失败: {e}");
                response::err(code::DATABASE, format!("预检失败: {e}"))
            }
        }
    } else {
        // 执行：事务内级联清理 + 删除
        match store.delete_with_cleanup(id).await {
            Ok(res) if res.deleted => response::ok(json!({
                "deleted": true,
                "cleanup": {
                    "sessions_unbound": res.sessions_unbound,
                    "memories_downgraded": res.memories_downgraded,
                    "proposals_removed": res.proposals_removed,
                },
            })),
            Ok(_) => response::err(code::NOT_FOUND, "助手不存在或为内置（不可删除）"),
            Err(e) => {
                tracing::error!(target: "assistant", "delete_assistant {id} 失败: {e}");
                response::err(code::DATABASE, format!("删除失败: {e}"))
            }
        }
    }
}

/// 把预检影响计数转成人类可读摘要，供前端确认框直接展示。
fn summarize_assistant_impact(
    impact: &crate::domain::assistant::store::AssistantDeletionImpact,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    if impact.sessions > 0 {
        parts.push(format!(
            "{} 个会话将解除助手绑定（回退默认助手）",
            impact.sessions
        ));
    }
    if impact.memories > 0 {
        parts.push(format!(
            "{} 条助手级记忆将降级为用户级（不丢失）",
            impact.memories
        ));
    }
    if impact.memory_proposals > 0 {
        parts.push(format!("{} 条记忆建议将被清理", impact.memory_proposals));
    }
    if parts.is_empty() {
        "无关联数据，可直接删除".to_string()
    } else {
        parts.join("；")
    }
}

/// 设置助手绑定的知识库实例（builtin/custom 均可，不受只读限制）
///
/// 供内置助手「配置知识库」用：内置助手整体只读，但知识库绑定是运行时配置，
/// 通过此接口单独更新 kb_instance_id，不影响其他只读字段。
pub async fn set_kb_instance(
    state: &AppState,
    assistant_id: &str,
    kb_instance_id: Option<&str>,
) -> Value {
    let store = match get_store(state) {
        Ok(s) => s,
        Err(v) => return v,
    };
    match store.set_kb_instance(assistant_id, kb_instance_id).await {
        Ok(true) => response::ok(json!({ "updated": true })),
        Ok(false) => response::err(code::NOT_FOUND, "助手不存在"),
        Err(e) => {
            tracing::error!(target: "assistant", "set_kb_instance {assistant_id} 失败: {e}");
            response::err(code::DATABASE, format!("更新失败: {e}"))
        }
    }
}

/// 复制助手为自定义副本
pub async fn duplicate_assistant(state: &AppState, id: &str) -> Value {
    let store = match get_store(state) {
        Ok(s) => s,
        Err(v) => return v,
    };
    let creator = current_creator(state);
    match store.duplicate_builtin(id, creator).await {
        Ok(new_id) => {
            tracing::info!(target: "assistant", "duplicate {id} → {new_id}");
            response::ok(json!({ "id": new_id }))
        }
        Err(e) => {
            tracing::error!(target: "assistant", "duplicate {id} 失败: {e}");
            let code_v = match &e {
                crate::error::AppError::BusinessError(_) => code::BUSINESS,
                _ => code::DATABASE,
            };
            business_err_obj(code_v, format!("{}", e))
        }
    }
}

/// 生成/续用分享口令
pub async fn share_enable(state: &AppState, id: &str) -> Value {
    let store = match get_store(state) {
        Ok(s) => s,
        Err(v) => return v,
    };
    let creator = current_creator(state);
    match store.get(id).await {
        Ok(Some(a)) => {
            if let Err((c, m)) = assert_writable(&a, creator) {
                return business_err_obj(c, m);
            }
        }
        Ok(None) => return response::err(code::NOT_FOUND, "助手不存在"),
        Err(e) => {
            tracing::error!(target: "assistant", "share_enable 查询 {id} 失败: {e}");
            return response::err(code::DATABASE, format!("查询失败: {e}"));
        }
    }
    match store.ensure_share_token(id).await {
        Ok(token) => {
            tracing::info!(target: "assistant", "share_enable {id} → token={token}");
            response::ok(json!({ "share_token": token }))
        }
        Err(e) => {
            tracing::error!(target: "assistant", "share_enable {id} 失败: {e}");
            response::err(code::DATABASE, format!("生成口令失败: {e}"))
        }
    }
}

/// 关闭分享口令
pub async fn share_disable(state: &AppState, id: &str) -> Value {
    let store = match get_store(state) {
        Ok(s) => s,
        Err(v) => return v,
    };
    let creator = current_creator(state);
    match store.get(id).await {
        Ok(Some(a)) => {
            if let Err((c, m)) = assert_writable(&a, creator) {
                return business_err_obj(c, m);
            }
        }
        Ok(None) => return response::err(code::NOT_FOUND, "助手不存在"),
        Err(e) => {
            tracing::error!(target: "assistant", "share_disable 查询 {id} 失败: {e}");
            return response::err(code::DATABASE, format!("查询失败: {e}"));
        }
    }
    match store.clear_share_token(id).await {
        Ok(_) => response::ok(json!({ "cleared": true })),
        Err(e) => {
            tracing::error!(target: "assistant", "share_disable {id} 失败: {e}");
            response::err(code::DATABASE, format!("关闭分享失败: {e}"))
        }
    }
}

/// Fork 公开/分享助手到本地
pub async fn fork_assistant(state: &AppState, id: &str) -> Value {
    let store = match get_store(state) {
        Ok(s) => s,
        Err(v) => return v,
    };
    let creator = current_creator(state);
    match store.fork(id, creator).await {
        Ok(new_id) => {
            tracing::info!(target: "assistant", "fork {id} → {new_id}");
            response::ok(json!({ "id": new_id }))
        }
        Err(e) => {
            tracing::error!(target: "assistant", "fork {id} 失败: {e}");
            let code_v = match &e {
                crate::error::AppError::BusinessError(_) => code::BUSINESS,
                _ => code::DATABASE,
            };
            business_err_obj(code_v, format!("{}", e))
        }
    }
}

/// 导出助手为 JSON
pub async fn export_one(state: &AppState, id: &str) -> Value {
    let store = match get_store(state) {
        Ok(s) => s,
        Err(v) => return v,
    };
    let creator = current_creator(state);
    match store.get(id).await {
        Ok(Some(a)) => {
            if let Err((c, m)) = assert_exportable(&a, creator) {
                return business_err_obj(c, m);
            }
            let visibility_str = match a.visibility {
                Visibility::Private => "private",
                Visibility::Shared => "shared",
                Visibility::Builtin => "builtin",
            };
            response::ok(json!({
                "schema": "cortex-agent.assistant.v1",
                "name": a.name,
                "description": a.description,
                "avatar": a.avatar,
                "kind": "custom",
                "agent_type": "custom",
                "visibility": visibility_str,
                "system_prompt": a.system_prompt,
                "model_id": a.model_id,
                "temperature": a.temperature,
                "top_p": a.top_p,
                "max_tokens": a.max_tokens,
                "enabled_tools": a.enabled_tools,
                "knowledge_enabled": a.knowledge_enabled,
                "kb_instance_id": a.kb_instance_id,
                "greeting": a.greeting,
            }))
        }
        Ok(None) => response::err(code::NOT_FOUND, "助手不存在"),
        Err(e) => {
            tracing::error!(target: "assistant", "export_one {id} 失败: {e}");
            response::err(code::DATABASE, format!("导出失败: {e}"))
        }
    }
}

/// 导入助手 JSON
pub async fn import_one(state: &AppState, v: &Value) -> Value {
    let store = match get_store(state) {
        Ok(s) => s,
        Err(v) => return v,
    };
    let schema = v.get("schema").and_then(|x| x.as_str()).unwrap_or("");
    if schema != "cortex-agent.assistant.v1" {
        return business_err_obj(
            code::INVALID_PARAMS,
            "schema 不兼容（仅支持 cortex-agent.assistant.v1）",
        );
    }
    let mut cleaned = v.clone();
    if let Some(obj) = cleaned.as_object_mut() {
        obj.remove("schema");
        obj.remove("kind");
        obj.remove("agent_type");
        obj.remove("visibility");
    }
    let mut req: WriteAssistantRequest = match serde_json::from_value(cleaned) {
        Ok(r) => r,
        Err(e) => {
            return business_err_obj(code::INVALID_PARAMS, format!("导入数据格式错误: {e}"));
        }
    };
    req.visibility = 0;
    if req.name.trim().is_empty() {
        req.name = "导入助手".to_string();
    }
    if req.avatar.trim().is_empty() {
        req.avatar = "🤖".to_string();
    }
    let input_data = match req.to_input() {
        Ok(i) => i,
        Err((c, m)) => return business_err_obj(c, m),
    };
    let creator = current_creator(state);
    match store.create_custom(&input_data, creator).await {
        Ok(new_id) => {
            tracing::info!(target: "assistant", "import_one → id={new_id}");
            response::ok(json!({ "id": new_id }))
        }
        Err(e) => {
            tracing::error!(target: "assistant", "import_one 失败: {e}");
            response::err(code::DATABASE, format!("导入失败: {e}"))
        }
    }
}

// 静态引用以避免未使用告警
const _KIND_REF: AssistantKind = AssistantKind::Builtin;
const _AGENT_TYPE_REF: AgentType = AgentType::Custom;

// ===========================================================================
// AI 智能生成助手草稿
// ===========================================================================

/// 依据用户模糊需求描述，让 LLM 自动生成助手的 name/description/system_prompt/greeting 四字段。
///
/// 只返回草稿，不落库；前端拿到后填充到编辑表单，用户可再编辑后再保存。
pub async fn generate_assistant(state: &AppState, input: &Value) -> Value {
    #[derive(Deserialize)]
    struct Req {
        prompt: String,
        #[serde(default)]
        model_id: Option<String>,
    }

    let req: Req = match serde_json::from_value(input.clone()) {
        Ok(r) => r,
        Err(e) => return response::err(code::PARSE_ERROR, format!("参数解析失败: {e}")),
    };
    if req.prompt.trim().is_empty() {
        return response::err(code::INVALID_PARAMS, "prompt 不能为空");
    }

    let model_id = req.model_id.as_deref().filter(|s| !s.trim().is_empty());
    let model = match state.require_model_store() {
        Ok(store) => match crate::llm::make_model_by_id(store, model_id) {
            Ok(m) => m,
            Err(e) => return response::err(code::LLM, format!("创建模型失败: {e}")),
        },
        Err(e) => return response::err(code::LLM, format!("创建模型失败: {e}")),
    };

    match crate::agent::assistant_generator::generate(model, &req.prompt).await {
        Ok(draft) => response::ok(json!({
            "name": draft.name,
            "description": draft.description,
            "system_prompt": draft.system_prompt,
            "greeting": draft.greeting,
        })),
        Err(e) => response::err(code::LLM, format!("生成失败: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_request_validates_empty_name() {
        let req = WriteAssistantRequest {
            name: "   ".into(),
            description: "".into(),
            avatar: "".into(),
            system_prompt: "".into(),
            model_id: "".into(),
            temperature: None,
            top_p: None,
            max_tokens: None,
            thinking_level: None,
            enabled_tools: vec![],
            knowledge_enabled: false,
            kb_instance_id: None,
            enabled_mcps: vec![],
            greeting: "".into(),
            visibility: 0,
        };
        let r = req.to_input();
        assert!(matches!(r, Err((c, _)) if c == code::INVALID_PARAMS));
    }

    #[test]
    fn write_request_sanitizes_tools() {
        let req = WriteAssistantRequest {
            name: "mybot".into(),
            description: "".into(),
            avatar: "".into(),
            system_prompt: "".into(),
            model_id: "".into(),
            temperature: None,
            top_p: None,
            max_tokens: None,
            thinking_level: None,
            enabled_tools: vec!["search_kb".into(), "browser".into(), "bogus".into()],
            knowledge_enabled: false,
            kb_instance_id: None,
            enabled_mcps: vec![],
            greeting: "".into(),
            visibility: 0,
        };
        let input = req.to_input().unwrap();
        assert_eq!(input.enabled_tools, vec!["search_kb".to_string()]);
    }

    #[test]
    fn write_request_rejects_invalid_visibility() {
        let req = WriteAssistantRequest {
            name: "x".into(),
            description: "".into(),
            avatar: "".into(),
            system_prompt: "".into(),
            model_id: "".into(),
            temperature: None,
            top_p: None,
            max_tokens: None,
            thinking_level: None,
            enabled_tools: vec![],
            knowledge_enabled: false,
            kb_instance_id: None,
            enabled_mcps: vec![],
            greeting: "".into(),
            visibility: 9,
        };
        let r = req.to_input();
        assert!(matches!(r, Err((c, _)) if c == code::INVALID_PARAMS));
    }
}
