//! 助手领域模型与 DB 行映射。
//!
//! - [`Assistant`]：纯领域模型（强类型枚举），不 derive serde，序列化由传输层 DTO 负责
//!   （`docs/architecture.md` §2.1 / 计划 A2）。
//! - [`AssistantRow`]：diesel 反序列化行，`enabled_tools` 以 TEXT 存 JSON
//!   （架构 §8.2 禁 JSONB，见计划 A3）。
//! - [`AssistantPublicCard`]：广场/分享脱敏视图（排除 `system_prompt`）。

use diesel::deserialize::QueryableByName;
use diesel::sql_types;
use serde::Serialize;

use super::enums::{AgentType, AssistantKind, Visibility};

/// 助手领域模型（业务层使用强类型枚举）。
#[derive(Debug, Clone)]
pub struct Assistant {
    pub id: String,
    pub name: String,
    pub description: String,
    pub avatar: String,
    pub kind: AssistantKind,
    pub agent_type: AgentType,
    pub system_prompt: String,
    pub model_id: String,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub max_tokens: Option<i32>,
    /// 思考级别：low/medium/high，None 表示不发（走模型默认）
    pub thinking_level: Option<String>,
    pub enabled_tools: Vec<String>,
    pub knowledge_enabled: bool,
    /// 绑定的知识库实例 id（多 provider；None=不绑定）
    pub kb_instance_id: Option<String>,
    /// 已启用的 MCP Server id 列表（JSON 数组存 TEXT，架构 §8.2）
    pub enabled_mcps: Vec<String>,
    pub greeting: String,
    pub share_token: String,
    pub fork_count: i32,
    pub creator: String,
    pub visibility: Visibility,
    pub sort_order: i32,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// 广场/分享脱敏卡片：不暴露 `system_prompt`、`share_token`、`creator` 等内部字段。
#[derive(Debug, Clone, Serialize)]
pub struct AssistantPublicCard {
    pub id: String,
    pub name: String,
    pub description: String,
    pub avatar: String,
    pub agent_type: AgentType,
    pub greeting: String,
    pub enabled_tools: Vec<String>,
    pub fork_count: i32,
}

impl From<&Assistant> for AssistantPublicCard {
    fn from(a: &Assistant) -> Self {
        Self {
            id: a.id.clone(),
            name: a.name.clone(),
            description: a.description.clone(),
            avatar: a.avatar.clone(),
            agent_type: a.agent_type,
            greeting: a.greeting.clone(),
            enabled_tools: a.enabled_tools.clone(),
            fork_count: a.fork_count,
        }
    }
}

/// 创建/更新自定义助手的领域层输入参数。
///
/// 不依赖传输层 DTO（架构 §2.1 / 计划 A2）：handler 把 Axum JSON DTO 转成本结构，
/// 再调 [`crate::domain::assistant::store::AssistantStore::create_custom`]。
///
/// `enabled_tools` 应由调用方先经
/// [`crate::tools::registry::sanitize_custom_tools`] 过滤。
#[derive(Debug, Clone)]
pub struct CustomAssistantInput {
    pub name: String,
    pub description: String,
    pub avatar: String,
    pub system_prompt: String,
    pub model_id: String,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub max_tokens: Option<i32>,
    /// 思考级别：low/medium/high，None 表示不发（走模型默认）
    pub thinking_level: Option<String>,
    pub enabled_tools: Vec<String>,
    pub knowledge_enabled: bool,
    /// 绑定的知识库实例 id（多 provider；None=不绑定）
    pub kb_instance_id: Option<String>,
    /// 已启用的 MCP Server id 列表（助手构建时注入对应 MCP 工具集）
    #[allow(dead_code)]
    pub enabled_mcps: Vec<String>,
    pub greeting: String,
    pub visibility: Visibility,
}

/// DB 行（枚举以 i16 落库；`enabled_tools` 以 TEXT 存 JSON 字符串）。
#[derive(Debug, Clone, QueryableByName)]
pub struct AssistantRow {
    #[diesel(sql_type = sql_types::Varchar)]
    pub id: String,
    #[diesel(sql_type = sql_types::Varchar)]
    pub name: String,
    #[diesel(sql_type = sql_types::Text)]
    pub description: String,
    #[diesel(sql_type = sql_types::Varchar)]
    pub avatar: String,
    #[diesel(sql_type = sql_types::Int2)]
    pub kind: i16,
    #[diesel(sql_type = sql_types::Int2)]
    pub agent_type: i16,
    #[diesel(sql_type = sql_types::Text)]
    pub system_prompt: String,
    #[diesel(sql_type = sql_types::Varchar)]
    pub model_id: String,
    #[diesel(sql_type = sql_types::Nullable<sql_types::Float8>)]
    pub temperature: Option<f64>,
    #[diesel(sql_type = sql_types::Nullable<sql_types::Float8>)]
    pub top_p: Option<f64>,
    #[diesel(sql_type = sql_types::Nullable<sql_types::Int4>)]
    pub max_tokens: Option<i32>,
    #[diesel(sql_type = sql_types::Nullable<sql_types::Text>)]
    pub thinking_level: Option<String>,
    #[diesel(sql_type = sql_types::Text)]
    pub enabled_tools: String,
    #[diesel(sql_type = sql_types::Bool)]
    pub knowledge_enabled: bool,
    #[diesel(sql_type = sql_types::Nullable<sql_types::Varchar>)]
    pub kb_instance_id: Option<String>,
    #[diesel(sql_type = sql_types::Text)]
    pub enabled_mcps: String,
    #[diesel(sql_type = sql_types::Text)]
    pub greeting: String,
    #[diesel(sql_type = sql_types::Varchar)]
    pub share_token: String,
    #[diesel(sql_type = sql_types::Int4)]
    pub fork_count: i32,
    #[diesel(sql_type = sql_types::Varchar)]
    pub creator: String,
    #[diesel(sql_type = sql_types::Int2)]
    pub visibility: i16,
    #[diesel(sql_type = sql_types::Int4)]
    pub sort_order: i32,
    #[diesel(sql_type = sql_types::Timestamptz)]
    pub created_at: chrono::DateTime<chrono::Utc>,
    #[diesel(sql_type = sql_types::Timestamptz)]
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

impl From<AssistantRow> for Assistant {
    fn from(r: AssistantRow) -> Self {
        let enabled_tools: Vec<String> = serde_json::from_str(&r.enabled_tools).unwrap_or_default();
        let enabled_mcps: Vec<String> = serde_json::from_str(&r.enabled_mcps).unwrap_or_default();
        Assistant {
            id: r.id,
            name: r.name,
            description: r.description,
            avatar: r.avatar,
            kind: AssistantKind::from_i16(r.kind),
            agent_type: AgentType::from_i16(r.agent_type),
            system_prompt: r.system_prompt,
            model_id: r.model_id,
            temperature: r.temperature,
            top_p: r.top_p,
            max_tokens: r.max_tokens,
            thinking_level: r.thinking_level,
            enabled_tools,
            knowledge_enabled: r.knowledge_enabled,
            kb_instance_id: r.kb_instance_id,
            enabled_mcps,
            greeting: r.greeting,
            share_token: r.share_token,
            fork_count: r.fork_count,
            creator: r.creator,
            visibility: Visibility::from_i16(r.visibility),
            sort_order: r.sort_order,
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_row() -> AssistantRow {
        AssistantRow {
            id: "01937000-0000-7000-8000-000000000001".into(),
            name: "Auto".into(),
            description: "d".into(),
            avatar: "🤖".into(),
            kind: 0,
            agent_type: 0,
            system_prompt: "secret".into(),
            model_id: "m".into(),
            temperature: Some(0.5),
            top_p: None,
            max_tokens: None,
            thinking_level: None,
            enabled_tools: r#"["search_kb","shell_command"]"#.into(),
            knowledge_enabled: true,
            kb_instance_id: None,
            enabled_mcps: r#"["01Habc","01Hdef"]"#.into(),
            greeting: "hi".into(),
            share_token: "TOK12345".into(),
            fork_count: 3,
            creator: "local".into(),
            visibility: 1,
            sort_order: 0,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn row_to_assistant_parses_enabled_tools_text() {
        let a: Assistant = sample_row().into();
        assert_eq!(a.enabled_tools, vec!["search_kb", "shell_command"]);
        assert_eq!(a.kind, AssistantKind::Builtin);
        assert_eq!(a.visibility, Visibility::Shared);
        assert_eq!(a.fork_count, 3);
    }

    #[test]
    fn row_to_assistant_handles_invalid_tools_json() {
        let mut r = sample_row();
        r.enabled_tools = "not json".into();
        let a: Assistant = r.into();
        assert!(a.enabled_tools.is_empty());
    }

    #[test]
    fn public_card_excludes_system_prompt_and_token() {
        let a: Assistant = sample_row().into();
        let card = AssistantPublicCard::from(&a);
        let json = serde_json::to_string(&card).unwrap();
        assert!(!json.contains("secret"));
        assert!(!json.contains("TOK12345"));
        assert!(json.contains("Auto"));
        assert!(json.contains("\"fork_count\":3"));
    }
}
