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

mod write;

pub use write::*;

/// 环境变量名上限（个数）。防滥用 + 控制注入体积。
const MAX_ENV_VARS: usize = 64;
/// 单个环境变量键/值长度上限。
const MAX_ENV_KEY_LEN: usize = 128;
const MAX_ENV_VALUE_LEN: usize = 8192;
/// 脱敏占位符：DTO 返回的 env_vars 值统一替换为此串（密钥级，键名仍可见）。
/// 真实明文仅由 [`reveal_env_vars`] 校验密码后返回。
const ENV_VALUE_MASK: &str = "••••••";

/// 校验环境变量名：首字符字母/下划线，其余字母/数字/下划线（POSIX env var 命名规则）。
fn is_valid_env_key(k: &str) -> bool {
    let mut chars = k.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && k.len() <= MAX_ENV_KEY_LEN
}

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
    /// 可用 Skill 白名单（存 skill name）。空 = 全部可见；非空 = 硬隔离仅列出的可见。
    /// 非法名（不符 ^[a-z0-9-]+$）被过滤丢弃；不存在的名字保留（容忍 skill 后续被删）。
    #[serde(default)]
    pub enabled_skills: Vec<String>,
    /// 助手级环境变量（JSON 对象 {"KEY":"VALUE"}）；会话执行 shell/脚本时注入子进程环境。
    /// 可能含密钥——不进公开 DTO/导出/fork。键名校验见 [`is_valid_env_key`]。
    /// `None`（缺省/null）= 更新时保持原值（脱敏编辑未解锁）；`Some` = 整体替换。
    #[serde(default)]
    pub env_vars: Option<std::collections::BTreeMap<String, String>>,
    #[serde(default)]
    pub greeting: String,
    #[serde(default = "default_visibility_private")]
    pub visibility: i16,
}

fn default_visibility_private() -> i16 {
    0
}

/// 清洗 skill 白名单：trim + 去空 + 过滤非法名（复用 skill 系统的命名校验），去重保序。
/// 不校验「是否真实存在」——容忍 skill 后续被删除，渲染时自动消失。
fn sanitize_enabled_skills(skills: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    skills
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && crate::domain::skill::is_valid_skill_name(s))
        .filter(|s| seen.insert(s.clone()))
        .collect()
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
        // 环境变量校验：仅当传入（Some）时校验。空值合法（env 允许空值）。
        if let Some(vars) = &self.env_vars {
            if vars.len() > MAX_ENV_VARS {
                return Err((
                    code::INVALID_PARAMS,
                    format!("环境变量数量超过上限 {}", MAX_ENV_VARS),
                ));
            }
            for (k, v) in vars {
                if !is_valid_env_key(k) {
                    return Err((
                        code::INVALID_PARAMS,
                        format!(
                            "非法的环境变量名: {k}（仅允许字母/数字/下划线，首字符须字母或下划线）"
                        ),
                    ));
                }
                if v.chars().count() > MAX_ENV_VALUE_LEN {
                    return Err((
                        code::INVALID_PARAMS,
                        format!("环境变量 {k} 的值过长（上限 {MAX_ENV_VALUE_LEN} 字符）"),
                    ));
                }
                // 防御纵深：拒绝把脱敏占位符 ENV_VALUE_MASK 当真实值回写。
                // 否则任何客户端把脱敏 DTO 原样回传，update 会加密掩码串覆盖真密钥（不可恢复）。
                if v == ENV_VALUE_MASK {
                    return Err((
                        code::INVALID_PARAMS,
                        format!(
                            "环境变量 {k} 的值是脱敏占位符；如需修改请先 reveal 明文或留空保持原值"
                        ),
                    ));
                }
            }
        }
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
            enabled_skills: sanitize_enabled_skills(&self.enabled_skills),
            env_vars: self.env_vars.clone(),
            greeting: self.greeting.clone(),
            visibility,
        })
    }
}

/// 把明文 env_vars 脱敏成键可见、值统一的掩码 map（DTO 返回用）。
fn mask_env_vars(
    vars: &std::collections::BTreeMap<String, String>,
) -> std::collections::BTreeMap<String, String> {
    vars.keys()
        .map(|k| (k.clone(), ENV_VALUE_MASK.to_string()))
        .collect()
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
    /// 可用 Skill 白名单（存 skill name）。空 = 全部可见。
    pub enabled_skills: Vec<String>,
    /// 助手级环境变量：**值已脱敏**（键名可见，值统一为 [`ENV_VALUE_MASK`]）。
    /// 真实明文须通过 [`reveal_env_vars`] 校验密码后获取。
    pub env_vars: std::collections::BTreeMap<String, String>,
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
            enabled_skills: a.enabled_skills,
            env_vars: mask_env_vars(&a.env_vars),
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

/// 调用者是否可改写该助手（敏感数据 / 增删改）。
///
/// `is_admin` 直通；否则须 `creator` 命中调用者。内置助手归属管理员 `marvelnet`（`seed_builtin`
/// 固定写入其 user_id），故管理员登录时 `creator` 天然命中；普通用户对内置/公开助手只读（见 [`can_read`]）。
fn caller_owns(a: &crate::domain::assistant::Assistant, user_id: &str, is_admin: bool) -> bool {
    is_admin || a.creator == user_id
}

/// 读可见性：归属人/管理员可见任意；否则须公开分享（visibility == Shared）。
/// 私有 custom 助手仅归属人/管理员可见——他人不可读（防越权窥探 system_prompt/env_vars）。
/// 内置助手（kind=0, visibility=Builtin）不再对全员公开，仅归属人（管理员 marvelnet）/管理员可读。
pub(crate) fn can_read(
    a: &crate::domain::assistant::Assistant,
    user_id: &str,
    is_admin: bool,
) -> bool {
    is_admin || a.creator == user_id || matches!(a.visibility, Visibility::Shared)
}

/// 改写校验：调用者归属（[`caller_owns`]）。内置助手数据驱动后与自定义同路径，
/// 归属人（管理员）可改写其 system_prompt / 工具 / 模型等。
fn assert_writable(
    a: &crate::domain::assistant::Assistant,
    user_id: &str,
    is_admin: bool,
) -> Result<(), (i32, String)> {
    if !caller_owns(a, user_id, is_admin) {
        return Err((code::BUSINESS, "无权操作他人创建的助手".into()));
    }
    Ok(())
}

/// 知识库绑定写权限：
/// - custom 助手：归属人/管理员可配置其知识库；
/// - 内置助手：归属管理员 `marvelnet`，仅管理员可配置其知识库。
///
/// 数据驱动后与 [`assert_writable`] 结果一致（均按归属鉴权），保留独立函数以表达
/// 「知识库绑定」语义；`set_kb_instance` 用此校验。
fn assert_kb_writable(
    a: &crate::domain::assistant::Assistant,
    user_id: &str,
    is_admin: bool,
) -> Result<(), (i32, String)> {
    if a.kind == AssistantKind::Custom {
        if !caller_owns(a, user_id, is_admin) {
            return Err((code::BUSINESS, "无权操作他人创建的助手".into()));
        }
    } else if !is_admin {
        return Err((code::BUSINESS, "仅管理员可配置内置助手的知识库".into()));
    }
    Ok(())
}

/// 校验待绑定知识库实例对调用者可见，防绑定他人私有知识库（运行时 search_kb 会读取，
/// 写时不校验则可跨用户读他人私有知识库）。`kb_id` 为 None/空白视为解绑，直接放行。
async fn validate_kb_readable(
    state: &AppState,
    kb_id: Option<&str>,
    user_id: &str,
    is_admin: bool,
) -> Result<(), Value> {
    let Some(kb_id) = kb_id.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(());
    };
    let kb_store = state.knowledge_manager.kb_instance_store();
    match kb_store.get(kb_id).await {
        Ok(Some(inst)) => {
            let readable = is_admin || inst.creator == user_id || inst.visibility > 0;
            if readable {
                Ok(())
            } else {
                Err(response::err(code::NOT_FOUND, "知识库实例不存在"))
            }
        }
        Ok(None) => Err(response::err(code::NOT_FOUND, "知识库实例不存在")),
        Err(e) => Err(response::err(
            code::DATABASE,
            format!("查询知识库失败: {e}"),
        )),
    }
}

/// 导出校验：归属（[`caller_owns`]）或公开可见。
fn assert_exportable(
    a: &crate::domain::assistant::Assistant,
    user_id: &str,
    is_admin: bool,
) -> Result<(), (i32, String)> {
    if caller_owns(a, user_id, is_admin) {
        return Ok(());
    }
    let public = matches!(a.visibility, Visibility::Shared);
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

/// 列出助手（按归属隔离）：普通用户=自己创建的 + 内置；管理员=全部。
pub async fn list_assistants(state: &AppState, user_id: &str, is_admin: bool) -> Value {
    let store = match get_store(state) {
        Ok(s) => s,
        Err(v) => return v,
    };
    match store.list_for_owner(user_id, is_admin).await {
        Ok(list) => {
            let dtos: Vec<AssistantDto> = list.into_iter().map(AssistantDto::from).collect();
            let mut items = serde_json::to_value(&dtos).unwrap_or_else(|_| json!([]));
            if let Some(arr) = items.as_array_mut() {
                super::owner::inject_owners(state.db_pool.as_ref(), is_admin, arr, "creator").await;
            }
            response::ok(json!({ "assistants": items }))
        }
        Err(e) => {
            tracing::error!(target: "assistant", "list_assistants 失败: {e}");
            response::err(code::DATABASE, format!("查询失败: {e}"))
        }
    }
}

/// 获取单个助手详情。私有助手仅归属人/管理员可读；否则返回 404（不暴露存在性）。
pub async fn get_assistant(state: &AppState, id: &str, user_id: &str, is_admin: bool) -> Value {
    let store = match get_store(state) {
        Ok(s) => s,
        Err(v) => return v,
    };
    match store.get(id).await {
        Ok(Some(a)) if can_read(&a, user_id, is_admin) => {
            response::ok(json!({ "assistant": AssistantDto::from(a) }))
        }
        Ok(_) => response::err(code::NOT_FOUND, "助手不存在"),
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
            enabled_skills: vec![],
            env_vars: Default::default(),
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
            enabled_skills: vec![],
            env_vars: Default::default(),
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
            enabled_skills: vec![],
            env_vars: Default::default(),
            greeting: "".into(),
            visibility: 9,
        };
        let r = req.to_input();
        assert!(matches!(r, Err((c, _)) if c == code::INVALID_PARAMS));
    }

    #[test]
    fn env_vars_validation_accepts_valid_and_rejects_bad_keys() {
        let mut good = std::collections::BTreeMap::new();
        good.insert("MY_API_KEY".to_string(), "sk-xxx".to_string());
        good.insert("_FOO".to_string(), String::new()); // 空值合法
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
            enabled_skills: vec![],
            env_vars: Some(good),
            greeting: "".into(),
            visibility: 0,
        };
        let input = req.to_input().unwrap();
        assert_eq!(
            input
                .env_vars
                .as_ref()
                .and_then(|m| m.get("MY_API_KEY"))
                .map(String::as_str),
            Some("sk-xxx")
        );

        // None（未传）= 保持原值，校验跳过，合法
        let req = WriteAssistantRequest {
            env_vars: None,
            ..write_req_fixture()
        };
        assert!(req.to_input().is_ok());

        // 非法键名（数字开头 / 含连字符）
        let mut bad = std::collections::BTreeMap::new();
        bad.insert("1ABC".to_string(), "v".to_string());
        let req = WriteAssistantRequest {
            env_vars: Some(bad),
            ..write_req_fixture()
        };
        assert!(matches!(req.to_input(), Err((c, _)) if c == code::INVALID_PARAMS));

        let mut bad2 = std::collections::BTreeMap::new();
        bad2.insert("MY-VAR".to_string(), "v".to_string());
        let req = WriteAssistantRequest {
            env_vars: Some(bad2),
            ..write_req_fixture()
        };
        assert!(matches!(req.to_input(), Err((c, _)) if c == code::INVALID_PARAMS));
    }

    #[test]
    fn mask_env_vars_hides_values_keeps_keys() {
        let mut vars = std::collections::BTreeMap::new();
        vars.insert("MY_API_KEY".to_string(), "sk-xxx".to_string());
        vars.insert("EMPTY".to_string(), String::new());
        let masked = mask_env_vars(&vars);
        assert_eq!(masked.len(), 2);
        // 键可见
        assert!(masked.contains_key("MY_API_KEY"));
        assert!(masked.contains_key("EMPTY"));
        // 值统一脱敏（不含明文）
        assert_eq!(
            masked.get("MY_API_KEY").map(String::as_str),
            Some(ENV_VALUE_MASK)
        );
        assert!(!masked.values().any(|v| v.contains("sk-xxx")));
    }

    #[test]
    fn to_input_rejects_mask_sentinel_value() {
        // 把脱敏占位符当真实值回写 → 拒（防止覆盖真密钥）
        let mut vars = std::collections::BTreeMap::new();
        vars.insert("MY_API_KEY".to_string(), ENV_VALUE_MASK.to_string());
        let req = WriteAssistantRequest {
            env_vars: Some(vars),
            ..write_req_fixture()
        };
        assert!(matches!(req.to_input(), Err((c, _)) if c == code::INVALID_PARAMS));
    }

    /// 测试夹具：最小合法 WriteAssistantRequest，供 env_vars 校验测试用 `..` 展开。
    fn write_req_fixture() -> WriteAssistantRequest {
        WriteAssistantRequest {
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
            enabled_skills: vec![],
            env_vars: Default::default(),
            greeting: "".into(),
            visibility: 0,
        }
    }
}
